use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Context;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::prelude::Widget;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use super::select::{Action, Selection};
use crate::tui::app::{App, AppContext, Control, Decide, MouseInput, ScreenKey, TUIAppView};
use crate::tui::dashboard::WHEEL_ROWS;
use crate::tui::log_sink::{LogSink, REPLAY_FLUSH_TIMEOUT, ReplayMark, Replayed, SinkLine};
use crate::tui::session::recorder::ViewTap;
use crate::tui::session::{SessionSlot, TappedReceiver};
use crate::tui::stderr_backend::StderrBackend;
use crate::tui::stop_arm::{self, CtrlC};
use hcore::events::now_unix_ms;
use hcore::shutdown::ShutdownTrigger;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);
// Wall-clock cap on how long a single tick's log drain may run — bounds
// redraw latency during a burst without ever dropping a line: the same `rx`
// persists across ticks, so anything left over just renders on the next one.
// Well under `TICK` so a capped drain still leaves room for the redraw.
const LOG_DRAIN_TICK_BUDGET: Duration = Duration::from_millis(20);

type StderrTerminal = Terminal<StderrBackend>;

/// Whether heph has the terminal on its alternate screen. Stored with
/// `Release` on each switch, read with `Acquire` by the hard abort's restore
/// closure ([`record_raw_mode_restore`]), which runs on whatever thread calls
/// `process::exit` and has no other way to know the screen is up.
static ALT_SCREEN_UP: AtomicBool = AtomicBool::new(false);

/// CSI ?1049l: back to the main screen, cursor restored to where `?1049h`
/// saved it. The byte string the hard abort writes with `libc::write`.
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";

/// Whether heph has the mouse captured, as [`ALT_SCREEN_UP`] is for the
/// screen: set (Release) before the capture is written, cleared (Release)
/// after it is turned off, read (Acquire) by the hard abort. A race costs
/// at worst one extra "off", never a capture left on.
static MOUSE_UP: AtomicBool = AtomicBool::new(false);

/// Button presses, and motion while a button is down (1002), reported in SGR
/// encoding (1006): the motion is what a drag selects with (see
/// [`super::select`]). Not crossterm's `EnableMouseCapture`, which also turns
/// on any-motion reporting (1003): an event for every cell the pointer
/// crosses, button or not.
///
/// Then alternate scroll (1007) off: a terminal with it on turns the wheel
/// into ↑/↓ on the alternate screen, which would move the selection instead
/// of scrolling the view. Most terminals already send wheel reports once
/// 1000 is on; this is for the ones that let 1007 win. Nothing turns it back
/// on at leave: whether it was on is learnt only by a query (DECRQM), and the
/// input stream is live by then. Mode 1007 only acts on the alternate
/// screen, so leaving it off changes nothing on the main screen; the cost
/// is a later full-screen program without mouse reporting (`less`) whose
/// wheel no longer scrolls in a terminal that had 1007 on.
///
/// 1000 goes first: a terminal that knows it but not 1002 still reports
/// clicks and the wheel. One that knows both takes the last one set.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1007l";
/// Every capture mode off; also what the hard abort writes.
const MOUSE_OFF: &[u8] = b"\x1b[?1002l\x1b[?1000l\x1b[?1006l";

/// Whether the full screens capture the mouse: yes, unless `HEPH_NO_MOUSE`
/// is set to anything but `0` or nothing. No config key (spec decision).
fn mouse_wanted() -> bool {
    std::env::var_os("HEPH_NO_MOUSE").is_none_or(|v| v.is_empty() || v == "0")
}

fn write_stderr(bytes: &[u8]) -> io::Result<()> {
    let mut e = io::stderr().lock();
    e.write_all(bytes)?;
    e.flush()
}

/// A full screen while it is up: the alternate screen, and a full-screen
/// terminal drawing on it.
///
/// It is the restore guard. Dropping it leaves the alternate screen, so every
/// way out of [`run`] that unwinds its stack — `q`, the end of the run, `?`,
/// and the `resume_unwind` of an app panic — hands the user back the main
/// screen and the scrollback on it as they were. The panic hook never touches
/// the alternate screen: it also fires for panics that are caught, under a
/// screen that stays up. The one exit with no destructors, the hard abort,
/// reads [`ALT_SCREEN_UP`] instead.
///
/// The inline terminal is kept, untouched, while this is up: rebuilding it
/// costs a cursor query, which must not run while the input stream is live.
struct FullScreen {
    terminal: StderrTerminal,
    /// The terminal was resized since the last draw: the next one repaints
    /// every cell, not only the ones that changed. See [`FullScreen::draw`].
    resized: bool,
    /// The mouse's selection: a left press held down freezes the frame,
    /// drawn in place of the view's until the release (see
    /// [`super::select`]).
    select: Selection,
    /// Whether the approval view was up at the press: a release-click on a
    /// screen that changed under the button is dropped, not matched against
    /// a hit map the user never saw.
    press_approval: bool,
    /// When the selection was last drawn from a motion report, to draw at
    /// most every [`SELECT_REPAINT`] (the tick catches up).
    painted: Instant,
    /// The note a copy leaves on the hint row, and when it was made.
    notice: Option<(String, Instant)>,
}

/// The least time between two draws of a selection from motion reports: a
/// burst of them over a slow link draws once, not once per report.
const SELECT_REPAINT: Duration = Duration::from_millis(16);
/// How long a copy's note stays on the hint row.
const NOTICE_FOR: Duration = Duration::from_millis(1500);
/// The hint row while a press is held.
const SELECTING_HINT: &str = " selecting · release to copy · any key cancels";

impl FullScreen {
    /// Switch to the alternate screen, and capture the mouse on it when
    /// `mouse`. A full-screen viewport asks the terminal nothing (its size
    /// is an ioctl), so this sends no query.
    fn enter(mouse: bool) -> anyhow::Result<Self> {
        let terminal = Terminal::with_options(
            StderrBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .context("building the full-screen terminal")?;
        // Each flag goes up before its write, so a hard abort in between
        // undoes a mode that may not be on yet rather than miss one that is.
        ALT_SCREEN_UP.store(true, Ordering::Release);
        // Built before the write: from here on dropping it is the restore,
        // the `?` below included.
        let screen = Self {
            terminal,
            resized: false,
            select: Selection::default(),
            press_approval: false,
            painted: Instant::now(),
            notice: None,
        };
        crossterm::execute!(io::stderr(), EnterAlternateScreen)
            .context("entering the alternate screen")?;
        if mouse {
            MOUSE_UP.store(true, Ordering::Release);
            write_stderr(MOUSE_ON).context("capturing the mouse")?;
        }
        Ok(screen)
    }

    /// Draw a frame. After a resize every cell is written, not only the
    /// ones that changed since the last draw.
    ///
    /// ratatui repaints in full only when the size it reads as it draws
    /// differs from the one it last drew at. Two resizes inside one tick
    /// that end at that size (a window shrunk and grown back, a zoom
    /// toggled) look like no resize to it, but the terminal reflowed its
    /// screen at the size in between — cut rows, pushed the top off — and
    /// a diff draw leaves that wreck up for good. `resize` to the current
    /// size clears the screen and drops ratatui's idea of what is on it;
    /// unlike `clear`, it sends no cursor query, which must not race the
    /// input stream.
    ///
    /// `lines` is the view's frame for the area. While a press is held it
    /// is not asked: the frozen frame is drawn instead, with the selection
    /// on it. A resize ends the drag, since the frame it froze no longer
    /// fits; so does [`select::IDLE`](super::select::IDLE) with no mouse
    /// report. A pointer held past a pane's edge scrolls it a step here,
    /// since a still pointer sends no report.
    fn draw(
        &mut self,
        now: Instant,
        lines: impl FnOnce(Rect) -> Vec<Line<'static>>,
    ) -> io::Result<()> {
        if std::mem::take(&mut self.resized) {
            self.select.cancel();
            let size = self.terminal.size()?;
            self.terminal.resize(size.into())?;
        }
        self.select.expire(now);
        self.select.tick(now);
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= NOTICE_FOR)
        {
            self.notice = None;
        }
        let notice = self.notice.as_ref().map(|(text, _)| text.as_str());
        let held = self.select.held();
        let mut misfit = false;
        self.terminal.draw(|fr| {
            let area = fr.area();
            match held {
                Some(d) if d.fits(area) => {
                    d.paint(fr.buffer_mut());
                    hint_row(fr.buffer_mut(), SELECTING_HINT);
                }
                _ => {
                    misfit = held.is_some();
                    fr.render_widget(Paragraph::new(Text::from(lines(area))), area);
                    if let Some(text) = notice {
                        hint_row(fr.buffer_mut(), text);
                    }
                }
            }
        })?;
        if misfit {
            self.select.cancel();
        }
        Ok(())
    }

    /// A left press: draw the view's frame now and return it, to freeze, so
    /// the drag that may follow selects from what is on screen.
    fn freeze(&mut self, lines: impl FnOnce(Rect) -> Vec<Line<'static>>) -> io::Result<Buffer> {
        self.select.cancel();
        if std::mem::take(&mut self.resized) {
            let size = self.terminal.size()?;
            self.terminal.resize(size.into())?;
        }
        let done = self.terminal.draw(|fr| {
            let area = fr.area();
            fr.render_widget(Paragraph::new(Text::from(lines(area))), area);
        })?;
        Ok(done.buffer.clone())
    }

    /// Draw the held selection now rather than on the next tick, at most
    /// every [`SELECT_REPAINT`]. Only the frozen frame is drawn, never the
    /// view's: a selection that no longer fits the screen ends, drawing
    /// nothing, and the next tick draws the view. A failed draw is logged:
    /// the next tick draws again.
    fn repaint_selection(&mut self, now: Instant) {
        if let Err(e) = self.try_repaint_selection(now) {
            tracing::debug!(error = %e, "drawing the selection");
        }
    }

    fn try_repaint_selection(&mut self, now: Instant) -> io::Result<()> {
        if now.saturating_duration_since(self.painted) < SELECT_REPAINT {
            return Ok(());
        }
        let area = Rect::from((Position::ORIGIN, self.terminal.size()?));
        if self.resized || !self.select.held().is_some_and(|d| d.fits(area)) {
            self.select.cancel();
            return Ok(());
        }
        self.painted = now;
        self.draw(now, |_| Vec::new())
    }

    /// The drag, if any, ends without copying: a key.
    fn cancel_selection(&mut self) {
        self.select.cancel();
    }
}

/// Write `text` over the frame's last row, the hint row: a selection's own
/// hints, or the note a copy leaves.
fn hint_row(buf: &mut Buffer, text: &str) {
    let area = buf.area;
    let Some(y) = area.bottom().checked_sub(1).filter(|&y| y >= area.top()) else {
        return;
    };
    for x in area.left()..area.right() {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.reset();
        }
    }
    buf.set_stringn(
        area.left(),
        y,
        text,
        usize::from(area.width),
        ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
    );
}

impl Drop for FullScreen {
    fn drop(&mut self) {
        // The mouse off before the screen is left, both written whether or
        // not they were turned on (harmless when off). Each flag is cleared
        // after its write, so a hard abort racing this drop writes a second
        // "off" at worst and never exits with either mode on.
        drop(write_stderr(MOUSE_OFF));
        MOUSE_UP.store(false, Ordering::Release);
        drop(crossterm::execute!(io::stderr(), LeaveAlternateScreen));
        ALT_SCREEN_UP.store(false, Ordering::Release);
        // A panic unwinding [`run`] on this thread: the hook printed it onto
        // the screen just left, so print it again where the user sees it.
        if std::thread::panicking()
            && let Some(text) = crate::tui::panic::take_for_current_thread()
        {
            print_panic_report(&text);
        }
    }
}

/// What a panic unwinding [`run`] must undo besides the alternate screen:
/// raw mode, the hidden cursor, and the sink holding heph's lines for a
/// terminal nobody draws any more. The panic hook used to do this, for every
/// panic, so a panic caught elsewhere (a tokio task's) left a live dashboard
/// reading keys line-buffered. Only an unwind reaches here; every other exit
/// tears the terminal down in order at the end of [`run`], and the hard abort
/// through `hcore::shutdown`'s restore closure.
struct UnwindRestore {
    sink: LogSink,
}

impl Drop for UnwindRestore {
    fn drop(&mut self) {
        if std::thread::panicking() {
            drop(disable_raw_mode());
            drop(crossterm::execute!(io::stderr(), crossterm::cursor::Show));
            self.sink.switch_to_direct();
        }
    }
}

/// Print a panic's report on the main screen, below the cursor the alternate
/// screen gave back. The line under it is stale now, so it is cleared. Raw
/// mode may still be on, so every line ends in an explicit carriage return.
fn print_panic_report(text: &str) {
    let mut out = String::with_capacity(text.len() + 16);
    out.push_str("\r\x1b[J");
    for line in text.lines() {
        out.push_str(line);
        out.push_str("\r\n");
    }
    let mut stderr = io::stderr().lock();
    drop(stderr.write_all(out.as_bytes()));
    drop(stderr.flush());
}

/// `HEPH_DEBUG_TUI_LOG=<file>`: while `<file>` exists, a thread writes one
/// numbered heph line every 40 ms — an `INFO`, a `DEBUG` under the
/// `heph_debug_tui_log` target, and a diagnostic, in turn — for the PTY test
/// that checks lines written under a full screen print once each, in order,
/// on return (`crates/bin-e2e`'s `tui_pty.rs`). In the shipped binary for the
/// same reason as [`DebugPanic`]; unset, it costs one environment read.
struct DebugLog {
    stop: std::sync::Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl DebugLog {
    fn from_env(sink: &LogSink) -> Option<Self> {
        let file = std::path::PathBuf::from(std::env::var_os("HEPH_DEBUG_TUI_LOG")?);
        tracing::warn!(
            var = "HEPH_DEBUG_TUI_LOG",
            "a test-only knob is set: this run writes test lines; unset it"
        );
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("heph-debug-tui-log".into())
            .spawn({
                let (stop, sink) = (std::sync::Arc::clone(&stop), sink.clone());
                move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::Acquire) {
                        if file.exists() {
                            n += 1;
                            match n % 3 {
                                0 => sink
                                    .write_diagnostic(&format!("heph-debug-log {n} diagnostic\n")),
                                1 => tracing::info!("heph-debug-log {n} info"),
                                _ => tracing::debug!(
                                    target: "heph_debug_tui_log",
                                    "heph-debug-log {n} debug"
                                ),
                            }
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    }
                }
            })
            .ok()?;
        Some(Self { stop, thread })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        drop(self.thread.join());
    }
}

/// `HEPH_DEBUG_TUI_PANIC`: make the run panic once it ends, for the PTY tests
/// that check the terminal comes back from a panic (`crates/bin-e2e`'s
/// `tui_pty.rs`). Those test the shipped binary, so the knob is in it; unset,
/// it costs one environment read per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugPanic {
    /// The app task panics a second after it returns: the `resume_unwind`
    /// path.
    App,
    /// The backend panics once the finished run has been held under the
    /// dashboard for half a second: the unwind path.
    Backend,
    /// A task of its own panics, once, as the first full screen goes up, and
    /// tokio catches it: the run goes on, and so must the screen and raw mode.
    Caught,
}

impl DebugPanic {
    fn from_env() -> Option<Self> {
        let knob = match std::env::var_os("HEPH_DEBUG_TUI_PANIC")?.to_str()? {
            "app" => Self::App,
            "backend" => Self::Backend,
            "caught" => Self::Caught,
            _ => return None,
        };
        tracing::warn!(
            var = "HEPH_DEBUG_TUI_PANIC",
            ?knob,
            "a test-only knob is set: this run panics on purpose; unset it"
        );
        Some(knob)
    }

    #[expect(clippy::panic, reason = "a deliberate panic is the knob's whole job")]
    fn fire(self) -> ! {
        panic!("HEPH_DEBUG_TUI_PANIC={self:?}: a deliberate panic")
    }
}

pub async fn run<A: App + 'static>(
    app: A,
    sink: LogSink,
    shutdown: ShutdownTrigger,
    slot: Option<&SessionSlot>,
    dashboard: bool,
) -> anyhow::Result<A::Output> {
    // The app owns its view: aggregation + rendering. It lives on the loop
    // stack so it survives pause/resume (only the terminal is rebuilt across a
    // pause cycle, not the view's aggregated state).
    let mut view = app.tui_view();
    if dashboard {
        // Up before the first frame: the first tick enters the full screen
        // before anything is drawn on the line.
        view.pin_dashboard();
    }
    // No start line here: the terminal is the TUI's. `heph session list` finds it.
    let session_tap = slot
        .and_then(|s| s.open(view.session_header(), true))
        .map(|s| s.tap);
    // After `open`, so the process's own session is never named as a holder.
    let holders = super::lock_holders(&app, slot);
    view.name_lock_holders(holders.clone());
    // Size the inline viewport to ~1/3 of the terminal height. Queried before the
    // viewport is built (so the first frame is already sized); the column count is
    // re-derived from the backend below.
    // Kept current from resize events: the view's row count depends on it.
    let mut term_height = crossterm::terminal::size()
        .map(|(_, h)| h)
        .unwrap_or(24)
        .max(1);
    // Mutable: recomputed (and the viewport rebuilt) on terminal resize so the
    // box stays ~1/3 of the live terminal height.
    let mut rows = view.rows(term_height);
    let mut rx = sink.switch_to_buffered();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    // We own the build-event channel: the sender goes to the app via
    // AppContext (and into its request state); we keep the receiver.
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut view_tap = session_tap.clone().map(|tap| ViewTap::new(tap, &view));
    let mut build_events: Option<TappedReceiver> =
        Some(TappedReceiver::new(event_rx, session_tap, holders));

    record_raw_mode_restore();
    enable_raw_mode().context("enabling raw mode")?;
    // Declared before the full screen's guard, so it drops after it: the
    // alternate screen is left first, in raw mode, as on every other exit.
    let _unwind = UnwindRestore { sink: sink.clone() };
    // StderrBackend wraps CrosstermBackend<Stderr> and overrides
    // get_cursor_position so the DSR query goes to stderr instead of
    // crossterm's hardcoded `io::stdout()`. Otherwise `cmd | wc -l`
    // sees the `\x1b[6n` bytes in its pipe and miscounts.
    let mut terminal = Terminal::with_options(
        StderrBackend::new(io::stderr()),
        TerminalOptions {
            viewport: Viewport::Inline(rows),
        },
    )
    .context("building inline terminal")?;
    terminal.autoresize()?;
    // The dashboard's screen while it is up; dropping it is the restore.
    let mut full: Option<FullScreen> = None;
    // Read once: whether the full screens capture the mouse.
    let mouse = mouse_wanted();
    let mut replay = Replay::default();

    let mut events: Option<EventStream> = Some(EventStream::new());
    let suppression = shutdown.suppression();

    // Shared with the app's request state: the engine registers fire-and-forget
    // sandbox cleanups against this counter. We keep rendering until the app
    // future resolves AND this drains, so the run visibly stays up while
    // background cleanups finish during exit (and the process doesn't tear the
    // cleaner thread out mid-rmdir).
    let bg_pending = hcore::pending::Pending::new_counter();
    let ctx = AppContext::with_control(
        sink.clone(),
        control_tx,
        Some(event_tx),
        std::sync::Arc::clone(&bg_pending),
    );
    // App runs on its own task so heavy sync work inside the app
    // (e.g. `block_in_place` for filesystem scans, Starlark eval) does
    // not block this task's ticker — the renderer task is re-polled
    // on another worker and the spinner keeps ticking.
    let debug_panic = DebugPanic::from_env();
    let mut caught_fired = false;
    let debug_log = DebugLog::from_env(&sink);
    let mut app_handle = tokio::spawn(async move {
        let out = app.run(ctx).await;
        if debug_panic == Some(DebugPanic::App) {
            // Long enough for the resume after the named target to put the
            // dashboard back: the panic is to land under a full screen.
            tokio::time::sleep(Duration::from_secs(1)).await;
            DebugPanic::App.fire();
        }
        out
    });

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut spinner_idx: usize = 0;
    let mut paused = false;
    // A resume received but not yet carried out. Rebuilding the viewport costs a
    // cursor query, one terminal round-trip, so the resume waits for the next
    // tick: a pause arriving first cancels it, and a run that ends first exits
    // still paused. The usual case is the pause that prints a run's results at
    // the very end, whose resume would rebuild a box only to tear it down again.
    let mut resume_pending = false;
    let mut cols = terminal_cols(&terminal);
    // Set when a resize event arrives; handled (coalesced) at the next tick so a
    // drag-resize burst triggers at most one re-anchor per tick instead of a
    // blocking DSR query per event.
    let mut needs_resize = false;

    // The app's result once its future resolves. We don't break the loop
    // there: the TUI stays up, rendering, until background cleanups drain too.
    let mut end: RunEnd<anyhow::Result<A::Output>> = RunEnd::new();
    let result: anyhow::Result<A::Output> = loop {
        // App finished and the background queue is empty — decide whether to
        // exit. A stop under way (a ctrl-c that stopped, or a signal: INT,
        // TERM, HUP) is never held: it ends the run on any screen.
        if let Some(r) = end.exit(bg_drained(&bg_pending), shutdown.fired(), &view) {
            break r;
        }
        tokio::select! {
            res = &mut app_handle, if !end.app_finished() => {
                let stopped = shutdown.fired();
                let res = match res {
                    Ok(inner) => inner,
                    Err(join_err) if join_err.is_panic() => {
                        let payload = join_err.into_panic();
                        // The hook printed this panic, on its own thread,
                        // onto the full screen if one is up: leave it, then
                        // print the panic again on the main screen.
                        if full.take().is_some() {
                            print_panic_report(&crate::tui::panic::take_for_payload(&*payload));
                        }
                        std::panic::resume_unwind(payload)
                    }
                    Err(join_err) => Err(anyhow::Error::new(join_err).context("app task")),
                };
                end.app_done(res, now_unix_ms(), stopped, &mut view);
            }
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(Control::Pause(ack, kind)) => {
                        // Leaving raw mode re-arms the terminal's ISIG, so from
                        // here until resume a Ctrl-C arrives as a real SIGINT
                        // instead of a key event. Suppress the trigger only when
                        // the pause is handing the keyboard to something else
                        // (`--shell`) — that session's Ctrl-C is not a request to
                        // cancel the build. For a plain pause (a diagnostic, a
                        // stdout flush, a target running with the terminal
                        // attached) the press is still heph's, and suppressing it
                        // left `heph run //pkg:target` with no working Ctrl-C at
                        // all for the whole of that target's run: the key handler
                        // is not reading (the event stream is dropped below), so
                        // the kernel SIGINT was the only surviving producer, and
                        // dropping it also put the second-press abort out of reach.
                        //
                        // Set even when already paused: a resume still pending is
                        // cancelled here, and this pause may own input where the
                        // last one did not.
                        suppression.set(kind.owns_input());
                        resume_pending = false;
                        if !paused {
                            // Leave the alternate screen first, with raw mode
                            // still on: the target gets the main screen, and
                            // the next tick after the resume puts the
                            // dashboard back, since the view still has it up.
                            // What it kept off the terminal is printed before
                            // the target writes anything.
                            if full.take().is_some() {
                                replay.start(&sink);
                            }
                            replay.finish(&mut terminal, cols).await;
                            // Drop the EventStream *before* `clear()`. `clear()`
                            // issues a cursor DSR query (`get_cursor_position`),
                            // which reads its reply through crossterm's single
                            // global reader; a live EventStream monopolises that
                            // reader and the query would deadlock (see
                            // `stderr_backend.rs`). The stream is rebuilt on resume.
                            events = None;
                            // Unbounded: `LogSink::switch_to_buffered` mints a fresh
                            // channel on resume, so anything left in `rx` here isn't
                            // deferred like a tick-path budget would be — it's
                            // orphaned when the old `rx` is dropped. Every byte must
                            // come out now.
                            drain_logs_to_terminal(&mut terminal, &mut rx, cols, None, replay.through);
                            // Collapse to the viewport origin (not `terminal.clear()`,
                            // which restores the live bottom-of-box cursor): the
                            // cooked-mode stdout write below must land at the box's
                            // top so the resume re-anchor reuses the cleared rows
                            // instead of stranding them as a blank gap.
                            collapse_inline_viewport(&mut terminal);
                            drop(terminal.show_cursor());
                            drop(disable_raw_mode());
                            flip_sink_to_direct_draining_the_gap(&sink, &mut rx, replay.through);
                            paused = true;
                        }
                        if ack.send(()).is_err() {
                            // Receiver dropped — app no longer waiting on pause ack.
                        }
                    }
                    Some(Control::Resume) if paused => {
                        // The terminal is rebuilt at the next tick (see
                        // `resume_pending`). The keyboard is heph's again now.
                        resume_pending = true;
                        suppression.set(false);
                    }
                    Some(Control::Resume) => {}
                    None => {}
                }
            }
            maybe_evt = async {
                match events.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            }, if !paused => {
                if let Some(Ok(ev)) = &maybe_evt
                    && stop_arm::disarms(ev)
                {
                    view.disarm_stop();
                }
                // A key ends a drag without copying: the key acts on the
                // view, which the frozen frame would otherwise hide.
                if let Some(Ok(Event::Key(_))) = &maybe_evt
                    && let Some(f) = full.as_mut()
                {
                    f.cancel_selection();
                }
                match maybe_evt {
                    Some(Ok(ev)) if stop_arm::is_ctrl_c(&ev) => {
                        // On a full screen the first press only arms; the
                        // view says which (`stop_arm`'s one rule).
                        let finished = end.app_finished();
                        if view.ctrl_c(now_unix_ms(), finished, shutdown.fired()) == CtrlC::Stop {
                            shutdown.trigger();
                            // Also quits a viewport held open after the run finished.
                            end.quit = true;
                        }
                    }
                    // `q` off the main view returns to it (like Esc); on the main
                    // view it quits a held (finished) viewport. Ignored mid-search
                    // (captured as query input below).
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('q'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if !view.is_searching() => {
                        if end.holds(&view) {
                            // The run ended under an open dashboard: `q`
                            // leaves, from whichever screen is up, once
                            // background work has drained.
                            end.quit = true;
                        } else if view.is_on_main_view() || view.quits_from_any_screen() {
                            if !end.app_finished() {
                                view.quit_while_running();
                            }
                        } else {
                            view.back_to_main();
                        }
                    }
                    // The approval view is the only place that decides:
                    // `y`/`n`/`Y`/`N` there, nowhere else (a `y` typed for
                    // something else never approves; `n` stays search's).
                    // These precede every other arm while it is up.
                    Some(Ok(Event::Key(key)))
                        if decision_key(&key, view.in_approval_view()).is_some() =>
                    {
                        if let Some(decide) = decision_key(&key, true) {
                            view.approval_decide(decide, now_unix_ms());
                        }
                    }
                    // ←/→ switch notices; Enter decides nothing there.
                    Some(Ok(Event::Key(KeyEvent {
                        code: code @ (KeyCode::Left | KeyCode::Right),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.in_approval_view() => view.tab(code == KeyCode::Right),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.in_approval_view() => {}
                    // Enter on the line's prompt row, and `v` on any full
                    // screen, open the oldest pending request.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if !view.full_screen() && !view.is_searching() => {
                        let _opened = view.approval_open();
                    }
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('v'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.full_screen() && !view.is_searching() => {
                        let _opened = view.approval_open();
                    }
                    // While the `/` filter captures input, printable keys, Backspace
                    // and Enter edit the query instead of firing the shortcuts
                    // below. These arms must precede the char shortcuts.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char(c),
                        modifiers,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.is_searching() && !modifiers.contains(KeyModifiers::CONTROL) => {
                        view.search_input(c)
                    }
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Backspace,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.is_searching() => view.search_backspace(),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.is_searching() => view.search_confirm(),
                    // Enter / `l` on a full screen's row: open that target.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.full_screen() => view.open_selected(false),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('l'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.full_screen() => view.open_selected(true),
                    // A full screen's own keys (the target Log tab's).
                    Some(Ok(Event::Key(KeyEvent {
                        code,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.full_screen() && screen_key(code).is_some() => {
                        if let Some(key) = screen_key(code) {
                            view.screen_key(key);
                        }
                    }
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Up,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.scroll(-1),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Down,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.scroll(1),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Tab,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.tab(true),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::BackTab,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.tab(false),
                    // `m` narrows the dashboard to the matched targets and
                    // back; the line has no scope to toggle.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('m'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.full_screen() => view.toggle_scope(),
                    // `/` opens the addr filter on the Done/Failed tabs (no-op on
                    // the live view). While searching it is captured as input above.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('/'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.search_start(),
                    // Esc clears the filter whether mid-type or already confirmed;
                    // with no filter active it returns to the main view.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Esc,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => {
                        if view.has_active_filter() {
                            view.search_cancel();
                        } else {
                            view.back_to_main();
                        }
                    }
                    // The mouse, captured only while a full screen is up: a
                    // report still in flight as it closed lands here with
                    // the line up, and is dropped.
                    Some(Ok(Event::Mouse(m))) => {
                        if let Some(f) = full.as_mut() {
                            on_mouse(f, &mut view, m);
                        }
                    }
                    Some(Ok(Event::Resize(w, h))) => {
                        // Cheap: just record. The terminal re-anchor (which does a
                        // DSR cursor query that must not race the EventStream) is
                        // deferred to the tick arm below.
                        cols = w.max(1);
                        term_height = h.max(1);
                        needs_resize = true;
                        if let Some(f) = full.as_mut() {
                            f.resized = true;
                            // Now, not at the next draw: a release before it
                            // would copy, or scroll the view, from rows laid
                            // out at the old size.
                            f.cancel_selection();
                        }
                    }
                    _ => {}
                }
            }
            // Not gated on `!paused` (A1): `apply` never draws, so folding while
            // the terminal is handed over costs nothing on screen, and a paused
            // run (an entrypoint, `--shell`) must keep recording its session.
            maybe_build_evt = async {
                match build_events.as_mut() {
                    // No per-event redraw: fold into the view here, the 80ms
                    // ticker repaints the pinned progress block.
                    Some(r) => r.recv(|e| view.apply(e)).await,
                    None => std::future::pending().await,
                }
            } => {
                if maybe_build_evt.is_none() {
                    // Sender dropped (request finished) — stop polling.
                    build_events = None;
                }
            }
            // The app is done and only background work is holding the exit:
            // wake on the last decrement instead of the next `TICK`.
            () = exit_drain_wake(end.app_finished(), &bg_pending) => {}
            _ = ticker.tick() => {
                // Deliberately NOT gated on `!paused`. While paused, the tick
                // draws nothing — the terminal belongs to the interactive
                // child — but it must keep firing as the loop's only
                // self-wake: with the event stream torn down and the ticker
                // off, the loop's sole remaining wake sources are the control
                // channel and the app `JoinHandle`, and both of those wakes
                // are issued from the app task, which by the end of a target
                // has usually gone through `block_in_place` (pluginexec's
                // macOS drain path) and lost its runtime core — so they ride
                // tokio's cross-thread wake (`mio::Waker` → kqueue
                // `EVFILT_USER`), observed to be droppable on macOS (see
                // `hproc::proc_exec` and `hcore::blocking`). One dropped
                // `Control::Resume` wake then left this loop parked forever
                // in a runtime with no timers: frame frozen mid-pause, engine
                // idle, process never exiting. A timer expiry is a kernel
                // timeout on the parked driver, not an `EVFILT_USER` trigger,
                // so the tick is the wake that cannot be lost; each one
                // re-enters the loop and the next `select!` re-polls the
                // control channel, observing any message whose wake vanished.
                //
                // Before the paused early-out: a label set while the terminal
                // is handed over ("Invoking …") is still the run's.
                if let Some(t) = view_tap.as_mut() {
                    t.observe(&view);
                }
                if paused {
                    if !resume_pending {
                        continue;
                    }
                    drop(enable_raw_mode());
                    // Cooked-mode writes during pause moved the cursor by an
                    // unknown amount; ratatui's internal state (viewport_area,
                    // last_known_cursor_pos, both buffers) is stale. Rebuild
                    // the terminal so `with_options`+`compute_inline_size`
                    // re-queries the cursor and positions the viewport below
                    // the printed output instead of clobbering it.
                    // The view may have changed shape while paused (an
                    // approval prompt came or went): rebuild at today's size.
                    let resumed_rows = view.rows(term_height);
                    if let Ok(new_term) = Terminal::with_options(
                        StderrBackend::new(io::stderr()),
                        TerminalOptions {
                            viewport: Viewport::Inline(resumed_rows),
                        },
                    ) {
                        terminal = new_term;
                        rows = resumed_rows;
                    }
                    events = Some(EventStream::new());
                    rx = sink.switch_to_buffered();
                    cols = terminal_cols(&terminal);
                    paused = false;
                    resume_pending = false;
                }
                // The approval view's flash, and a request that left the
                // queue: either can close it, so before the check below.
                view.advance(now_unix_ms());
                // The view opened or closed its full screen.
                if view.full_screen() != full.is_some() {
                    if full.take().is_some() {
                        // Back on the main screen, as the terminal saved it.
                        // The full screen's terminal showed the cursor as it
                        // dropped; the line keeps it hidden.
                        drop(terminal.hide_cursor());
                        replay.start(&sink);
                    } else if !replay.pending() {
                        // Opened again before the last replay printed: it
                        // opens once that has, so the mark below starts
                        // after it.
                        //
                        // Every line queued until now is printed before the
                        // screen goes up; from here on each is dropped as it
                        // arrives, and the replay brings it back.
                        let (queued, mark) = sink.replay_mark(&mut rx);
                        for line in queued.iter().filter(|l| l.seq > replay.through) {
                            print_log_bytes(&mut terminal, &line.bytes, cols);
                        }
                        match FullScreen::enter(mouse) {
                            Ok(f) => {
                                full = Some(f);
                                replay.mark = Some(mark);
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "full screen unavailable");
                                view.back_to_main();
                            }
                        }
                    }
                }
                spinner_idx = (spinner_idx + 1) % SPINNER_FRAMES.len();
                let frame = SPINNER_FRAMES.get(spinner_idx).copied().unwrap_or("");
                if let Some(f) = full.as_mut() {
                    // heph's lines are not printed under a full screen, nor
                    // held: the sink numbered and recorded each one as it was
                    // sent, and the replay reads them back on return.
                    while rx.try_recv().is_ok() {}
                    // A resize while the full screen is up re-anchors the
                    // inline rows once back on the line: `needs_resize`
                    // stays set. The full-screen terminal sizes itself from
                    // the ioctl as it draws, and repaints every cell after
                    // a resize.
                    let now = now_unix_ms();
                    drop(f.draw(Instant::now(), |area| {
                        view.render_full(now, area.width, area.height)
                    }));
                    // Under the held dashboard, once it has been up a while
                    // (so a test sees it before the panic takes it down).
                    if debug_panic == Some(DebugPanic::Backend)
                        && end.held
                        && end.at.is_some_and(|at| now.saturating_sub(at) >= 500)
                    {
                        DebugPanic::Backend.fire();
                    }
                    if debug_panic == Some(DebugPanic::Caught) && !caught_fired {
                        caught_fired = true;
                        // The handle is dropped: nobody resumes the panic.
                        drop(tokio::spawn(async { DebugPanic::Caught.fire() }));
                    }
                    continue;
                }
                // The view changed shape (a prompt came or went): re-anchor
                // through the same path a resize takes, which tears the event
                // stream down across the cursor query.
                if view.rows(term_height) != rows {
                    needs_resize = true;
                }
                if needs_resize {
                    // Re-anchor the inline viewport at the new size before drawing.
                    // The tick is a synchronous draw-owning point, so the
                    // EventStream teardown/restore inside has no `.await` between
                    // them — the no-race invariant holds (see stderr_backend.rs).
                    reanchor_after_resize(&mut terminal, &mut events, &mut cols, &mut rows, |h| {
                        view.rows(h)
                    });
                    needs_resize = false;
                }
                // Budgeted: `rx` persists across ticks (only pause/resume ever
                // replace it), so anything left over after the budget trips just
                // renders on the next tick — deferred, never dropped. A replay
                // still running holds live lines back, so they print after it.
                if replay.ready() {
                    replay.finish(&mut terminal, cols).await;
                }
                if !replay.pending() {
                    drain_logs_to_terminal(
                        &mut terminal,
                        &mut rx,
                        cols,
                        Some(LOG_DRAIN_TICK_BUDGET),
                        replay.through,
                    );
                }
                let lines = view.render(frame, now_unix_ms(), cols, rows);
                drop(terminal.draw(|f| {
                    let area = f.area();
                    f.render_widget(Paragraph::new(Text::from(lines)), area);
                }));
            }
        }
    };

    // The run can finish while the TUI is paused (a target holding the terminal,
    // a `BufferedStdout` flush, the pause that prints the results), resume still
    // pending or not. The pause already did this teardown: it
    // drained `rx`, collapsed the box, showed the cursor, left raw mode and
    // flipped the sink to direct writes. The cursor sits just below whatever
    // the target wrote, which is where the summary belongs. Rebuilding a
    // terminal here only to tear it down again cost a cursor query at exit:
    // one terminal round-trip on every `heph r //pkg:target`, up to its
    // 2s timeout on a terminal that never answers.
    //
    // Back to the main screen first (a pause already left it), so the
    // teardown and the summary below land in the scrollback.
    if let Some(d) = debug_log {
        d.stop();
    }
    if full.take().is_some() {
        drop(terminal.hide_cursor());
        replay.start(&sink);
    }
    if !paused {
        // What the screen kept off the terminal, before the lines after it.
        replay.finish(&mut terminal, cols).await;
        // Flush any still-buffered build-event logs into the terminal scrollback
        // *above* the viewport. This must stay on the terminal path (`insert_before`):
        // it wraps to the viewport width and skips blank lines. The post-teardown
        // `drain_logs_to_stderr` fallback writes raw bytes, so letting empties fall
        // through to it dumps stray newlines after the box.
        drain_logs_to_terminal(&mut terminal, &mut rx, cols, None, replay.through);
        // Collapse the viewport to its origin so the final summary (printed below)
        // lands where the box started, not below it.
        collapse_inline_viewport(&mut terminal);
        {
            let backend = terminal.backend_mut();
            drop(backend.show_cursor());
            drop(Backend::flush(backend));
        }
        drop(disable_raw_mode());
    }
    hcore::shutdown::clear_terminal_restore();
    sink.switch_to_direct();
    drain_logs_to_stderr(&mut rx, replay.through);

    // The app completing breaks the loop, but build events emitted just before
    // it returned may still be buffered (the sender closes only once the
    // request state drops). Drain them so the final summary reflects the full
    // stream rather than the snapshot at the last tick.
    if let Some(r) = build_events.as_mut() {
        r.drain(|e| view.apply(e));
    }

    // Persistent final summary, printed straight to stderr below the
    // torn-down inline viewport (interactive mode only). Measured at the
    // instant the app ended, the clock the screens froze at; recorded as
    // `Finished` so a viewer prints the same line, and so it knows what
    // follows was printed below it.
    let at = end.at.unwrap_or_else(now_unix_ms);
    if end.stopped {
        view.set_stopped();
    }
    view.set_finished(at);
    if let Some(t) = view_tap.as_mut() {
        t.finished(&view, at);
    }
    view.last_render();

    result
}

/// A captured mouse event on the full screen `f`. A left press freezes the
/// frame; a drag selects on it, and scrolls the pane it started in when the
/// wheel turns or the pointer is held past the pane's edge; the release
/// copies the selection and says so on the hint row, or is a click when
/// there was no drag. A wheel step that is not a drag's ends any drag and
/// scrolls the view. Other buttons and modified presses mean nothing.
fn on_mouse(f: &mut FullScreen, view: &mut impl TUIAppView, m: MouseEvent) {
    let (col, row) = (m.column, m.row);
    let at = Position::new(col, row);
    let now = Instant::now();
    match m.kind {
        // Not as the screen closes (esc or `q` went in before the tick
        // takes it down): there is no frame of it to freeze.
        MouseEventKind::Down(MouseButton::Left) if m.modifiers.is_empty() && view.full_screen() => {
            let ms = now_unix_ms();
            match f.freeze(|area| view.render_full(ms, area.width, area.height)) {
                Ok(frozen) => {
                    let doc = view.selection_doc(at);
                    f.select.press(frozen, doc, at, now);
                    f.press_approval = view.in_approval_view();
                }
                Err(e) => tracing::debug!(error = %e, "freezing the frame for a selection"),
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if f.select.motion(at, now) == Action::Repaint {
                f.repaint_selection(now);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let released = f.select.release(at);
            // Not on a screen that changed under the button: a click or a
            // scroll is matched against what the user saw.
            let same_screen = view.in_approval_view() == f.press_approval;
            if let Some(top) = released.scrolled.filter(|_| same_screen) {
                view.selection_scrolled(top);
            }
            match released.action {
                Action::Click(p) if same_screen => {
                    view.mouse(MouseInput::Click { col: p.x, row: p.y });
                }
                Action::Copy(text) => {
                    let sent = capped(&text);
                    match copy_to_clipboard(sent) {
                        Ok(()) => f.notice = Some((copy_note(sent, &text), now)),
                        Err(e) => tracing::debug!(error = %e, "copying the selection"),
                    }
                }
                Action::Click(_) | Action::Repaint | Action::Nothing => {}
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = m.kind == MouseEventKind::ScrollDown;
            let rows = if down { WHEEL_ROWS } else { -WHEEL_ROWS };
            match f.select.wheel(rows, now) {
                Some(Action::Repaint) => f.repaint_selection(now),
                Some(_) => {}
                None => view.mouse(MouseInput::Wheel { col, row, down }),
            }
        }
        _ => {}
    }
}

/// The most text one copy sends, in bytes: about 87 KB once base64'd into
/// the OSC 52. Some terminals silently drop a much larger one while the
/// note would still say "copied"; past this, the copy is cut and the note
/// says so.
const COPY_CAP: usize = 64 * 1024;

/// `text` cut to [`COPY_CAP`] bytes, on a character boundary.
fn capped(text: &str) -> &str {
    text.get(..text.floor_char_boundary(COPY_CAP))
        .unwrap_or(text)
}

/// The hint-row note for a copy that sent `sent` of the selected `text`.
fn copy_note(sent: &str, text: &str) -> String {
    let n = sent.chars().count();
    let what = if sent.len() < text.len() {
        format!("the first {n} of {} chars", text.chars().count())
    } else {
        format!("{n} char{}", if n == 1 { "" } else { "s" })
    };
    format!(
        " copied {what} (OSC 52) · not on your clipboard? HEPH_NO_MOUSE=1 gives the terminal its own selection"
    )
}

/// Put `text` on the system clipboard through the terminal: an OSC 52
/// ("set the clipboard selection to this base64"). The terminal decides
/// whether to honour it; one that does not (or a multiplexer not set to
/// pass it on, as tmux's `set-clipboard` default) drops it, and nothing
/// says so — heph cannot learn the outcome without a query racing the
/// input stream.
fn copy_to_clipboard(text: &str) -> io::Result<()> {
    crossterm::execute!(
        io::stderr(),
        crossterm::clipboard::CopyToClipboard::to_clipboard_from(text)
    )
}

/// The decision `key` asks for: `y`/`n`/`Y`/`N` pressed (without ctrl)
/// while the approval view is up. Anywhere else no key decides, so `n`
/// stays a log search's next match.
pub(crate) fn decision_key(key: &KeyEvent, in_approval_view: bool) -> Option<Decide> {
    if !in_approval_view
        || key.kind != KeyEventKind::Press
        || key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Decide::of(c),
        _ => None,
    }
}

/// The full-screen key `code` stands for, if any.
pub(crate) fn screen_key(code: KeyCode) -> Option<ScreenKey> {
    Some(match code {
        KeyCode::Char('f') => ScreenKey::Follow,
        KeyCode::Char('g') | KeyCode::Home => ScreenKey::Top,
        KeyCode::Char('G') | KeyCode::End => ScreenKey::Bottom,
        KeyCode::Char('w') => ScreenKey::Wrap,
        KeyCode::Char('n') => ScreenKey::Next,
        KeyCode::Char('N') => ScreenKey::Prev,
        KeyCode::PageUp => ScreenKey::PageUp,
        KeyCode::PageDown => ScreenKey::PageDown,
        _ => return None,
    })
}

/// Whether background work no longer holds the exit: the loop's break
/// predicate once the app has finished.
fn bg_drained(bg_pending: &hcore::pending::Pending) -> bool {
    bg_pending.load(std::sync::atomic::Ordering::Acquire) == 0
}

/// The run loop's drain arm: resolves on the decrement that drains the
/// background work, but only once the app has finished and work is still
/// outstanding. Otherwise it never resolves, so a held viewport (count
/// already zero, waiting for `q`) does not spin the loop and a running app
/// is not woken by every drain mid-run.
async fn exit_drain_wake(app_done: bool, bg_pending: &hcore::pending::Pending) {
    if app_done && !bg_drained(bg_pending) {
        bg_pending.wait_drained().await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// The run's end, as the loop sees it: the app's result once its future
/// resolved, and whether the finished run is held on screen.
struct RunEnd<R> {
    /// The app's result, from the moment its future resolved until the
    /// loop leaves with it.
    result: Option<R>,
    /// When the app's future resolved: the instant the run ended.
    at: Option<u64>,
    /// A stop (a ctrl-c that stopped, or a signal: INT, TERM, HUP) had
    /// reached the run when the app ended: the final line says "stopped".
    /// Read at that instant, so the ctrl-c that quits a held, finished run
    /// is not one.
    stopped: bool,
    /// Latched once the finished run is held open on a full screen: the
    /// viewport keeps rendering until the user quits explicitly.
    held: bool,
    /// Set by `q` / Ctrl-C to break out of a held viewport.
    quit: bool,
}

impl<R> RunEnd<R> {
    fn new() -> Self {
        Self {
            result: None,
            at: None,
            stopped: false,
            held: false,
            quit: false,
        }
    }

    /// The app's future resolved at `at` with `result`: the run is over,
    /// whatever background work (sandbox removal, cache uploads) is still
    /// draining. The view says so now: the dashboard's title takes the
    /// verdict and every clock on screen stops at `at`, rather than counting
    /// on, verdict-less, until the drain lets the loop leave.
    fn app_done(&mut self, result: R, at: u64, stopped: bool, view: &mut impl TUIAppView) {
        if stopped {
            view.set_stopped();
        }
        view.set_finished(at);
        self.result = Some(result);
        self.at = Some(at);
        self.stopped = stopped;
    }

    /// Whether the app has finished: its result is in hand.
    fn app_finished(&self) -> bool {
        self.result.is_some()
    }

    /// Whether the finished run is, or will be once background work
    /// drains, held on screen: `q` then leaves from whichever screen is up.
    fn holds(&self, view: &impl TUIAppView) -> bool {
        self.held || (self.app_finished() && view.hold_after_finish())
    }

    /// The app's result once the loop is to leave with it: background work
    /// drained (`drained`), and the run not held on screen. A run ended on
    /// a full screen is held until an explicit quit, a stop under way
    /// (`stopping`), or the user closing the dashboard back to the line.
    fn exit(&mut self, drained: bool, stopping: bool, view: &impl TUIAppView) -> Option<R> {
        if !drained || self.result.is_none() {
            return None;
        }
        if self.held {
            if self.quit || stopping || !view.hold_after_finish() {
                return self.result.take();
            }
            return None;
        }
        if view.hold_after_finish() && !stopping && !self.quit {
            self.held = true;
            return None;
        }
        self.result.take()
    }
}

/// Capture stderr's pre-raw-mode `termios` and register a restore closure
/// with `hcore::shutdown` before raw mode mutates it. The closure is the
/// only thing that still runs if the run ends via `bootstrap`'s hard abort
/// on the second Ctrl-C (`std::process::exit`, which skips destructors) — it
/// restores cooked mode and re-shows the cursor with direct syscalls only,
/// never crossterm: the abort path must not risk contending crossterm's own
/// raw-mode mutex, which a concurrently-aborting thread could already hold.
///
/// A failed `tcgetattr` (stderr is not a tty — output redirected) records
/// nothing, so a non-interactive run's hard abort stays a no-op: no stray
/// escape bytes land in redirected output.
fn record_raw_mode_restore() {
    let fd = io::stderr().as_raw_fd();
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is stderr's descriptor, valid for the process lifetime;
    // `termios.as_mut_ptr()` points at valid, correctly-sized storage for
    // `tcgetattr` to write into.
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc != 0 {
        return;
    }
    // SAFETY: `tcgetattr` returned success above, so `termios` is initialized.
    let termios = unsafe { termios.assume_init() };
    hcore::shutdown::set_terminal_restore(move || {
        // Nothing else reaches the terminal from here to `process::exit`:
        // the render loop keeps drawing on its own thread until then, and a
        // frame flushed after the bytes below hides the cursor again (and
        // can draw the dashboard over the main screen). Every frame goes
        // through `io::Stderr`, so holding its lock, never released, stops
        // them; a frame being written finishes first. The writes below are
        // `libc::write`s, which do not take it. (A task, not a signal
        // handler: allocating here is fine.)
        let _held: &'static mut io::StderrLock<'static> = Box::leak(Box::new(io::stderr().lock()));
        // The dashboard's mouse capture and alternate screen first:
        // `process::exit` runs no destructor, so [`FullScreen`]'s drop never
        // will. A capture left on would turn every click in the user's
        // shell into escape codes; leaving the screen puts the main screen
        // and its scrollback back before the notice below.
        if MOUSE_UP.load(Ordering::Acquire) {
            // SAFETY: as for the notice below — a valid pointer for exactly
            // its own length, and a short write is ignored.
            unsafe {
                libc::write(fd, MOUSE_OFF.as_ptr().cast(), MOUSE_OFF.len());
            }
        }
        if ALT_SCREEN_UP.load(Ordering::Acquire) {
            // SAFETY: as for the notice below — a valid pointer for exactly
            // its own length, and a short write is ignored.
            unsafe {
                libc::write(fd, LEAVE_ALT_SCREEN.as_ptr().cast(), LEAVE_ALT_SCREEN.len());
            }
        }
        // SAFETY: `fd` was a valid, open tty when captured above; `tcsetattr`
        // on an already-closed fd fails harmlessly (EBADF), not UB.
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &termios);
        }
        // Cursor-show escape, then a plain notice on its own line: the TUI
        // buffers `tracing` output through its log sink and only drains it
        // to the terminal on the next render tick — a tick the hard abort
        // never reaches — so without this direct write the process just
        // vanishes with no visible sign it force-killed itself rather than
        // finishing normally.
        let notice = b"\r\n\x1b[?25hheph: aborted (second Ctrl-C)\r\n";
        // SAFETY: `notice` is a valid pointer for exactly its own length;
        // the return value is intentionally ignored — this runs right
        // before `process::exit`, so there is nothing left to do with a
        // short write.
        unsafe {
            libc::write(fd, notice.as_ptr().cast(), notice.len());
        }
    });
}

fn terminal_cols(terminal: &StderrTerminal) -> u16 {
    terminal.size().map(|r| r.width).unwrap_or(80).max(1)
}

/// Re-anchor an inline viewport to the backend's current size. `autoresize()`
/// re-runs `compute_inline_size`, which issues a DSR cursor query via the
/// backend. Kept generic and free of the EventStream choreography so it can be
/// exercised with a `TestBackend`.
fn reanchor_terminal<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
    drop(terminal.autoresize());
}

/// Re-anchor after a terminal resize, keeping the inline viewport at ~1/3 of the
/// new terminal height. The DSR query (inside `autoresize`, and again when we
/// rebuild the terminal for a new row count) reads its reply through crossterm's
/// shared reader; the `EventStream` monopolises that reader, so it must be torn
/// down across the query (see `stderr_backend.rs`). There must be no `.await`
/// between the two `events` writes — callers run this from the synchronous tick
/// arm.
fn reanchor_after_resize(
    terminal: &mut StderrTerminal,
    events: &mut Option<EventStream>,
    cols: &mut u16,
    rows: &mut u16,
    rows_for_height: impl Fn(u16) -> u16,
) {
    *events = None;
    reanchor_inline(terminal, rows, rows_for_height, |desired, _collapsed| {
        Terminal::with_options(
            StderrBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Inline(desired),
            },
        )
        .ok()
    });
    *events = Some(EventStream::new());
    // The backend size ratatui actually re-anchored to is the source of truth.
    *cols = terminal_cols(terminal);
}

/// Re-anchor the inline viewport to the backend's current size. `rows_for_height`
/// is the view's own sizing policy — the same one that sized the viewport at
/// startup, so a resize can't quietly swap a bespoke view onto the progress
/// view's ~1/3-of-height rule. Generic over the backend and over how a new
/// terminal is built, so the ordering below can be exercised with a `TestBackend`;
/// `reanchor_after_resize` layers the EventStream choreography on top.
///
/// Two things must happen for a resize to land without artifacts:
///
/// 1. `reanchor_terminal` (`autoresize`) — ratatui recomputes the inline origin,
///    erases the viewport region and resets the back buffer, so the next `draw`
///    is a full repaint rather than a diff against cells laid out at the old
///    width. Without it the box is patched in place and the stale tail of every
///    row survives the resize.
/// 2. If the row count changed, the terminal is rebuilt so the viewport reserves
///    the new height — but the old viewport must be collapsed away *first*.
///    `autoresize` restores the pre-resize cursor, which sits at the *bottom* of
///    the live box; a rebuilt terminal anchors its viewport wherever the cursor
///    is, so skipping the collapse walks the box down the screen on every resize
///    and strands the rows it used to occupy as a blank gap. The collapse parks
///    the cursor back at the box's origin so the new viewport reuses those rows.
///    Same reason the pause/resume rebuild collapses.
fn reanchor_inline<B: Backend>(
    terminal: &mut Terminal<B>,
    rows: &mut u16,
    rows_for_height: impl Fn(u16) -> u16,
    rebuild: impl FnOnce(u16, &mut Terminal<B>) -> Option<Terminal<B>>,
) {
    reanchor_terminal(terminal);
    let term_height = terminal.size().map(|r| r.height).unwrap_or(24).max(1);
    let desired = rows_for_height(term_height);
    if desired == *rows {
        return;
    }
    collapse_inline_viewport(terminal);
    if let Some(new_term) = rebuild(desired, terminal) {
        *terminal = new_term;
        *rows = desired;
    }
}

/// Collapse the inline viewport: erase the box and leave the cursor at the
/// viewport's origin (top-left). Used at pause, at exit, and before a
/// resize-driven terminal rebuild, so whatever is drawn next — a cooked-mode
/// stdout flush, the final summary, or the re-anchored box — lands where the box
/// started rather than below it.
///
/// Unlike [`ratatui::Terminal::clear`], this issues no cursor DSR query (so it
/// can't race crossterm's reader) and does not restore the live bottom-of-box
/// cursor. Restoring the live cursor is what stranded the cleared rows as a
/// blank gap: on pause, the stdout write then landed at the box's bottom and the
/// resume re-anchored a full viewport-height lower. The empty `draw` diffs the
/// box away; `Frame::area().y` is ratatui's own inline anchor.
fn collapse_inline_viewport<B: Backend>(terminal: &mut Terminal<B>) {
    let mut anchor_row: u16 = 0;
    drop(terminal.draw(|f| anchor_row = f.area().y));
    let backend = terminal.backend_mut();
    drop(backend.set_cursor_position(Position {
        x: 0,
        y: anchor_row,
    }));
    drop(backend.clear_region(ClearType::AfterCursor));
}

/// Drain buffered log bytes into the terminal's scrollback. `budget` bounds how
/// long a single call may run: `None` drains to completion (required at the
/// pause and exit call sites, where `rx` is about to be replaced or torn down
/// and anything left behind would be lost, not deferred); `Some(d)` stops once
/// `d` has elapsed, leaving the remainder in `rx` for the next call — safe only
/// where the same `rx` is guaranteed to be drained again (the tick path).
///
/// The budget is checked between messages, never mid-message: a message already
/// taken off the channel is always rendered in full, so a call never abandons
/// partially-processed data.
///
/// A line numbered at most `through` is dropped: a replay printed it already.
fn drain_logs_to_terminal<B: Backend>(
    terminal: &mut Terminal<B>,
    rx: &mut mpsc::UnboundedReceiver<SinkLine>,
    cols: u16,
    budget: Option<Duration>,
    through: u64,
) {
    let deadline = budget.map(|d| Instant::now() + d);
    while let Ok(SinkLine { seq, bytes }) = rx.try_recv() {
        if seq > through {
            print_log_bytes(terminal, &bytes, cols);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
}

/// One of heph's lines into the scrollback above the inline viewport,
/// wrapped to `cols`; blank lines are skipped.
fn print_log_bytes<B: Backend>(terminal: &mut Terminal<B>, bytes: &[u8], cols: u16) {
    let cols = cols.max(1);
    let text = bytes
        .into_text()
        .unwrap_or_else(|_| Text::raw(String::from_utf8_lossy(bytes).into_owned()));
    for line in text.lines {
        let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
        if width == 0 {
            continue;
        }
        let rows = rows_needed(width, cols);
        drop(terminal.insert_before(rows, move |buf: &mut Buffer| {
            let area = buf.area;
            Paragraph::new(line)
                .wrap(Wrap { trim: false })
                .render(area, buf);
        }));
    }
}

fn rows_needed(width: u16, cols: u16) -> u16 {
    let cols = cols.max(1);
    if width == 0 {
        return 1;
    }
    let rows = u32::from(width).div_ceil(u32::from(cols));
    u16::try_from(rows).unwrap_or(u16::MAX).max(1)
}

/// Like [`drain_logs_to_terminal`], raw onto stderr.
fn drain_logs_to_stderr(rx: &mut mpsc::UnboundedReceiver<SinkLine>, through: u64) {
    let mut stderr = io::stderr().lock();
    while let Ok(SinkLine { seq, bytes }) = rx.try_recv() {
        if seq > through {
            drop(stderr.write_all(&bytes));
        }
    }
}

/// Flip `sink` to direct writes without losing anything still queued.
/// `LogSink::switch_to_buffered` mints a brand-new channel on the next resume,
/// so a line written between the pause-time terminal drain and this flip —
/// which lands in the same still-buffered `rx`, since the sink hasn't flipped
/// yet — must be caught here or it is orphaned when `rx` is replaced, not
/// merely deferred like a tick-path budget would defer it. `switch_to_direct`
/// is a happens-before boundary: once it returns, no writer can reach `rx`
/// again, so draining right after it is guaranteed to catch the whole gap.
/// Mirrors the double-drain the exit path already relies on.
fn flip_sink_to_direct_draining_the_gap(
    sink: &LogSink,
    rx: &mut mpsc::UnboundedReceiver<SinkLine>,
    through: u64,
) {
    sink.switch_to_direct();
    drain_logs_to_stderr(rx, through);
}

/// heph's lines a full screen kept off the terminal, printed again when it
/// closes (I3). While a screen is up the backend holds only `mark`, never a
/// line's text; the lines come back from the session's `events.ndjson`.
#[derive(Default)]
struct Replay {
    /// Where the full screen that is up took the terminal over.
    mark: Option<ReplayMark>,
    /// The replay of the screen last closed, running off the render task:
    /// it waits for the recorder's flush and reads the file.
    running: Option<tokio::task::JoinHandle<(Vec<Vec<u8>>, Replayed)>>,
    /// The last line a replay printed. A live line numbered at most this
    /// was printed by it, and is dropped.
    through: u64,
}

impl Replay {
    /// The screen closed: replay what it kept off the terminal.
    fn start(&mut self, sink: &LogSink) {
        if let Some(mark) = self.mark.take() {
            let sink = sink.clone();
            self.running = Some(tokio::task::spawn_blocking(move || {
                let mut lines = Vec::new();
                let replayed = sink.replay(mark, REPLAY_FLUSH_TIMEOUT, |bytes| {
                    lines.push(bytes.to_vec())
                });
                (lines, replayed)
            }));
        }
    }

    /// A replay is still running: live lines wait behind it, and no screen
    /// opens until it is done.
    fn pending(&self) -> bool {
        self.running.is_some()
    }

    fn ready(&self) -> bool {
        self.running.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Wait for the running replay, if any, and print its lines.
    async fn finish<B: Backend>(&mut self, terminal: &mut Terminal<B>, cols: u16) {
        let Some(handle) = self.running.take() else {
            return;
        };
        match handle.await {
            Ok((lines, replayed)) => {
                for bytes in &lines {
                    print_log_bytes(terminal, bytes, cols);
                }
                self.through = self.through.max(replayed.through);
            }
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => tracing::debug!(error = %e, "log replay task"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COPY_CAP, IntoText, RunEnd, TICK, bg_drained, capped, copy_note, exit_drain_wake,
        rows_needed,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn a_copy_under_the_cap_is_sent_whole() {
        let text = "a".repeat(COPY_CAP);
        assert_eq!(capped(&text), text);
        assert!(copy_note(capped(&text), &text).starts_with(" copied 65536 chars (OSC 52)"));
        assert!(copy_note("x", "x").starts_with(" copied 1 char (OSC 52)"));
        assert_eq!(capped(""), "");
    }

    #[test]
    fn a_copy_over_the_cap_is_cut_on_a_char_and_says_so() {
        // 3-byte chars: the cap falls inside one, so the cut lands before it.
        let text = "漢".repeat(COPY_CAP / 3 + 10);
        let sent = capped(&text);
        assert_eq!(sent.len(), COPY_CAP / 3 * 3);
        assert!(text.starts_with(sent));
        let note = copy_note(sent, &text);
        assert!(
            note.starts_with(&format!(
                " copied the first {} of {} chars (OSC 52)",
                COPY_CAP / 3,
                COPY_CAP / 3 + 10
            )),
            "{note}"
        );
    }

    /// Regression: while the app future does CPU-bound sync work between
    /// awaits (e.g. `block_in_place` for a filesystem walk or Starlark
    /// eval), the render ticker must keep firing. Pre-fix the app and the
    /// renderer shared a `tokio::select!` task; a sync chunk inside the
    /// app starved the ticker. Post-fix the app is `tokio::spawn`ed so
    /// the renderer task is re-polled on another worker.
    ///
    /// This mirrors the architecture of `interactive::run` (spawn the app,
    /// drive a ticker alongside) without pulling in a real terminal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn renderer_ticks_while_app_blocks_in_place() {
        const BLOCK_MS: u64 = 400;
        let app = tokio::spawn(async {
            tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(BLOCK_MS)));
        });
        tokio::pin!(app);

        let ticks = Arc::new(AtomicUsize::new(0));
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                res = &mut app => { res.expect("app task"); break; }
                _ = interval.tick() => { ticks.fetch_add(1, Ordering::SeqCst); }
            }
        }

        // 400 ms at TICK=80 ms ⇒ ~5 ticks. Allow slack for scheduler
        // jitter and the initial immediate tick; require ≥3 to clearly
        // distinguish from pre-fix behaviour (which would yield 0–1).
        let observed = ticks.load(Ordering::SeqCst);
        assert!(
            observed >= 3,
            "renderer must keep ticking while app is in block_in_place; got {observed} ticks during {BLOCK_MS} ms"
        );
    }

    /// Regression for the darwin-only CI hang in `bin-e2e`'s
    /// `tui_renders_the_run_and_restores_the_terminal`: the run finished
    /// engine-side (stall watchdog reported `open=0`), the frame stayed frozen
    /// at the pre-pause render, and the process never exited.
    ///
    /// While the TUI is paused for an interactive target, the event stream is
    /// torn down and the ticker used to be gated off (`, if !paused`), so the
    /// loop's only remaining wake sources were the control channel and the app
    /// `JoinHandle`. Both of those wakes are issued by the app task, which by
    /// the end of a target has typically passed through `block_in_place`
    /// (pluginexec's macOS drain path) and lost its runtime core, so they ride
    /// tokio's cross-thread wake — kqueue `EVFILT_USER`, observed to drop on
    /// macOS (see `hproc::proc_exec`, `hcore::blocking`). One dropped
    /// `Control::Resume` wake left the loop parked in a runtime with no timers
    /// registered at all: nothing could ever poll it again. The fix keeps the
    /// ticker armed while paused (drawing nothing) as a wake the kernel
    /// timeout delivers, so a message whose wake vanished is still observed on
    /// the next tick's re-poll of the select.
    ///
    /// A genuinely dropped tokio wake cannot be forced from a test, so — like
    /// the other tests in this module — this mirrors the loop's shape rather
    /// than driving `run` itself (which needs a real terminal). The dropped
    /// wake is modelled precisely: state that is observable by polling but
    /// whose change produces *no* wake (an atomic flipped from a plain OS
    /// thread), next to a control arm that never wakes. With the pre-fix shape
    /// (tick arm gated on `!paused`) this loop has no wake source left and
    /// hangs until the timeout; with the ticker armed it converges within a
    /// few frames. What it does not prove: that `run`'s real arm carries no
    /// gate — that line is covered by review, not by this mirror.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paused_loop_keeps_ticking_so_a_wakeless_resume_is_still_observed() {
        use std::sync::atomic::AtomicBool;

        // "A Resume is queued, but the wake that should announce it was
        // dropped": observable state, zero waker traffic.
        let resume_queued = Arc::new(AtomicBool::new(false));
        {
            let resume_queued = Arc::clone(&resume_queued);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                resume_queued.store(true, Ordering::Release);
            });
        }

        let mut paused = true;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    // The control channel whose wake never arrives.
                    () = std::future::pending::<()>() => {}
                    _ = interval.tick() => {
                        // Mirrors the fix: the tick arm is NOT gated on
                        // `!paused`; while paused it only re-enters the loop,
                        // which is what lets the next poll observe the queued
                        // resume.
                        if paused && resume_queued.load(Ordering::Acquire) {
                            paused = false;
                        }
                        if !paused {
                            break;
                        }
                    }
                }
            }
        })
        .await;

        assert!(
            outcome.is_ok(),
            "a paused loop whose ticker is gated off has no wake source left \
             once a cross-thread wake is dropped; the tick must stay armed \
             while paused"
        );
    }

    /// A paused run keeps recording its session (A1): an entrypoint or `--shell`
    /// holds the terminal for the whole of its run, and `heph session attach`
    /// must keep seeing what it builds meanwhile.
    ///
    /// Two halves, because `run` itself needs a real terminal. This one drives
    /// the real recording path (`Session`, its tap, `TappedReceiver`) through a
    /// loop shaped like `run`'s: a `Control::Pause` is taken first, then every
    /// build event sent after it must reach `events.ndjson`.
    /// `run_does_not_gate_the_build_event_arm_on_pause` checks that `run`'s
    /// own build-event arm carries no pause guard, the edit that would stop
    /// recording while this loop still passed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_loop_still_records_build_events() {
        use crate::tui::app::{Control, PauseFor};
        use crate::tui::session::{
            HeaderKind, HolderNames, RecordReader, Session, SessionHeader, SessionRecord,
            SessionRoot, SessionStart, TappedReceiver, files,
        };
        use hcore::events::{BuildEvent, BuildEventKind};
        use tokio::sync::{mpsc, oneshot};

        const SENT: usize = 5;
        let root = tempfile::tempdir().expect("tempdir");
        let session = Session::open(
            &SessionRoot::new(root.path()),
            SessionStart {
                header: SessionHeader {
                    kind: HeaderKind::Build,
                    label: "run //a:shell".into(),
                },
                ..SessionStart::default()
            },
        )
        .expect("open");
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut build_events =
            TappedReceiver::new(event_rx, Some(session.tap()), HolderNames::default());
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();

        // The app: pause the TUI, and only once the pause is acknowledged emit
        // the events, so every one of them arrives while paused.
        let app = tokio::spawn(async move {
            let (ack, acked) = oneshot::channel();
            control_tx
                .send(Control::Pause(ack, PauseFor::Input))
                .expect("pause");
            acked.await.expect("ack");
            for i in 0..SENT {
                let addr = format!("//a:t{i}");
                event_tx
                    .send(BuildEvent {
                        at_unix_ms: 1,
                        kind: BuildEventKind::ResultStart { addr },
                    })
                    .expect("send");
            }
            // Dropping both senders ends the loop below.
        });

        let mut paused = false;
        let mut folded_while_paused = 0;
        let mut control_open = true;
        let looped = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    ctrl = control_rx.recv(), if control_open => match ctrl {
                        Some(Control::Pause(ack, _)) => {
                            paused = true;
                            ack.send(()).expect("ack");
                        }
                        Some(Control::Resume) => paused = false,
                        None => control_open = false,
                    },
                    // As in `run`: no pause guard on this arm.
                    got = build_events.recv(|_| {
                        if paused {
                            folded_while_paused += 1;
                        }
                    }) => if got.is_none() {
                        break;
                    },
                }
            }
        })
        .await;
        assert!(looped.is_ok(), "the paused loop never drained the events");
        app.await.expect("app");
        drop(build_events);
        session.finish(Some(0), false);

        assert_eq!(folded_while_paused, SENT, "every event folded while paused");
        let file = std::fs::File::open(session.dir().join(files::EVENTS)).expect("events");
        let mut reader = RecordReader::new(file);
        let recorded = std::iter::from_fn(|| reader.next_record().expect("read"))
            .filter(|r: &SessionRecord| r.build_event().is_some())
            .count();
        assert_eq!(
            recorded, SENT,
            "every build event emitted while the TUI was paused is recorded"
        );
    }

    /// `run`'s build-event arm must not be guarded on `paused`: with the guard,
    /// a paused run (an entrypoint, `--shell`) stops recording its session for
    /// as long as it holds the terminal. `run` needs a real terminal, so this
    /// reads its source; `a_paused_loop_still_records_build_events` proves
    /// the unguarded shape records.
    #[test]
    fn run_does_not_gate_the_build_event_arm_on_pause() {
        let src = include_str!("interactive.rs");
        let (_, arm) = src
            .split_once("maybe_build_evt = async {")
            .expect("run's build-event arm");
        let (head, _) = arm.split_once("=> {").expect("the arm's body");
        assert!(
            !head.contains("paused"),
            "run's build-event arm is gated on `paused`, so a paused run stops \
             recording its session:\n{head}"
        );
    }

    /// The exit loop must not break the moment the app future resolves: it keeps
    /// rendering until the shared background-cleanup counter drains to zero. It
    /// drives the same break predicate (`bg_drained`) and drain arm
    /// (`exit_drain_wake`) that `run` uses, without a real terminal.
    ///
    /// It also exits on the decrement itself rather than on the next `TICK`:
    /// under paused time the loop returns at exactly the drain instant (120 ms),
    /// where waiting for the ticker would return at 160 ms.
    #[tokio::test(start_paused = true)]
    async fn loop_stays_open_until_bg_pending_drains() {
        const DRAIN_AT: Duration = Duration::from_millis(120);
        // App finishes immediately, but leaves 1 unit of background work that a
        // separate task clears after a delay.
        let bg_pending = hcore::pending::Pending::new_counter();
        bg_pending.fetch_add(1, Ordering::AcqRel);
        let app = tokio::spawn(async { 7u8 });
        tokio::pin!(app);

        let started = tokio::time::Instant::now();
        let drainer = {
            let bg = Arc::clone(&bg_pending);
            tokio::spawn(async move {
                tokio::time::sleep(DRAIN_AT).await;
                bg.fetch_sub(1, Ordering::AcqRel);
            })
        };

        let mut app_result: Option<u8> = None;
        let mut ticks_after_app = 0usize;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let result = loop {
            if let Some(r) = app_result.take() {
                if bg_drained(&bg_pending) {
                    break r;
                }
                app_result = Some(r);
            }
            tokio::select! {
                res = &mut app, if app_result.is_none() => {
                    app_result = Some(res.expect("app task"));
                }
                () = exit_drain_wake(app_result.is_some(), &bg_pending) => {}
                _ = interval.tick() => {
                    if app_result.is_some() {
                        ticks_after_app += 1;
                    }
                }
            }
        };

        drainer.await.expect("drainer");
        assert_eq!(result, 7, "must return the app's value");
        assert!(
            ticks_after_app >= 1,
            "loop must keep rendering after the app finished while bg work drains; got {ticks_after_app} ticks"
        );
        assert_eq!(
            started.elapsed(),
            DRAIN_AT,
            "the loop must exit on the drain, not on the next tick"
        );
    }

    /// The drain arm stays silent unless it has something to wake for: not
    /// while the app is still running (even with work outstanding), and not
    /// once the count is already zero (a held viewport would otherwise spin).
    #[tokio::test(start_paused = true)]
    async fn drain_arm_only_resolves_after_the_app_with_work_outstanding() {
        let silent = Duration::from_secs(5);
        let bg = hcore::pending::Pending::new_counter();

        // Count zero, app done: a held viewport — must not resolve.
        assert!(
            tokio::time::timeout(silent, exit_drain_wake(true, &bg))
                .await
                .is_err(),
            "a zero count with the app done must not wake the loop"
        );

        bg.fetch_add(1, Ordering::AcqRel);
        // App still running: a drain mid-run is not an exit wake.
        let running = exit_drain_wake(false, &bg);
        tokio::pin!(running);
        assert!(
            tokio::time::timeout(silent, running.as_mut())
                .await
                .is_err(),
            "the arm must stay silent while the app runs"
        );

        // App done, work outstanding: resolves exactly at the decrement.
        let started = tokio::time::Instant::now();
        let drainer = {
            let bg = Arc::clone(&bg);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                bg.fetch_sub(1, Ordering::AcqRel);
            })
        };
        exit_drain_wake(true, &bg).await;
        assert_eq!(started.elapsed(), Duration::from_millis(30));
        drainer.await.expect("drainer");
    }

    /// Mirrors the held-viewport logic in `interactive::run`: when the app
    /// finishes on a non-main view (`hold_after_finish == true`), the loop must
    /// keep rendering and only break once an explicit quit (`q` / Ctrl-C) is
    /// observed — never auto-exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn held_viewport_waits_for_explicit_quit() {
        let app = tokio::spawn(async { 7u8 });
        tokio::pin!(app);

        let mut app_result: Option<u8> = None;
        let mut held = false;
        let mut quit = false;
        // Simulate the user being off the main view at finish time.
        let hold_after_finish = true;
        let mut ticks_after_finish = 0usize;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let result = loop {
            if let Some(r) = app_result.take() {
                // Background queue drained (== 0) immediately in this model.
                if held {
                    if quit {
                        break r;
                    }
                    app_result = Some(r);
                } else if hold_after_finish {
                    held = true;
                    app_result = Some(r);
                } else {
                    break r;
                }
            }
            tokio::select! {
                res = &mut app, if app_result.is_none() => {
                    app_result = Some(res.expect("app task"));
                }
                _ = interval.tick() => {
                    if held {
                        ticks_after_finish += 1;
                        // After a few held frames, the user presses `q`.
                        if ticks_after_finish == 3 {
                            quit = true;
                        }
                    }
                }
            }
        };

        assert_eq!(result, 7, "must return the app's value");
        assert!(
            ticks_after_finish >= 3,
            "held viewport must keep rendering until explicit quit; got {ticks_after_finish}"
        );
    }

    /// The run ends when the app does, not when its background work (sandbox
    /// removal, cache uploads) has drained: from that instant the dashboard's
    /// title carries the verdict and every clock on screen stops, the line's
    /// included. Regression: both waited for the drain, so a run whose
    /// targets were all done kept counting, verdict-less, for as long as its
    /// sandboxes took to remove.
    #[test]
    fn the_run_ends_on_screen_when_the_app_does_not_when_background_work_drains() {
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
        use crate::tui::progress::test_events::{T0, at, end, exec, start};

        for (error, verdict) in [(None, "✓ done"), (Some("exit 3"), "✗ 1 failed")] {
            let mut view = TuiProgressView::new("Running //p:t");
            view.apply(&at(0, start("//p:t")));
            view.apply(&at(0, exec("//p:t")));
            view.apply(&at(2_000, end("//p:t", error, None)));
            view.open_dashboard();
            let title =
                |v: &mut TuiProgressView, now: u64| v.render_full(now, 100, 14)[0].to_string();
            let line = |v: &TuiProgressView, now: u64| v.render("⠋", now, 100, 1)[0].to_string();

            let mut run = RunEnd::new();
            run.app_done((), T0 + 2_000, false, &mut view);
            // Background work outstanding: the loop stays up.
            assert_eq!(run.exit(false, false, &view), None);
            let shown = title(&mut view, T0 + 7_000);
            assert!(shown.contains(verdict), "no {verdict:?} in {shown:?}");
            assert!(shown.contains(" 2s "), "clock not frozen at 2s: {shown:?}");
            assert_eq!(
                title(&mut view, T0 + 60_000),
                shown,
                "the clock kept counting"
            );
            assert_eq!(
                line(&view, T0 + 7_000),
                line(&view, T0 + 60_000),
                "the line's clock kept counting"
            );
            // `q` leaves even before the drain: the run is over.
            assert!(run.holds(&view));

            // Drained: held on the dashboard, as it was, until `q`.
            assert_eq!(run.exit(true, false, &view), None);
            assert_eq!(title(&mut view, T0 + 90_000), shown);
            run.quit = true;
            assert_eq!(run.exit(true, false, &view), Some(()));
        }
    }

    /// Regression: resizing the terminal broke inline-viewport rendering. The
    /// `Event::Resize` arm only updated the local width; the viewport itself was
    /// never re-anchored in a window where the DSR cursor query could run safely,
    /// so the next `draw()` raced crossterm's EventStream on /dev/tty and the box
    /// rendered garbled / at the stale width.
    ///
    /// The real /dev/tty race needs a PTY and injected timing to reproduce and is
    /// not honestly unit-testable here. This freezes the observable contract the
    /// fix restores: after `reanchor_terminal`, the inline viewport tracks the new
    /// backend size and the next frame lays out at the new width — using a
    /// `TestBackend` whose deterministic cursor lets `compute_inline_size` run
    /// without a tty.
    #[test]
    fn resize_reanchors_inline_viewport_and_reflows() {
        use super::reanchor_terminal;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Position;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;
        use ratatui::{TerminalOptions, Viewport};

        fn row_string(buf: &Buffer, y: u16) -> String {
            (buf.area.left()..buf.area.right())
                .filter_map(|x| buf.cell(Position::new(x, y)))
                .map(|c| c.symbol())
                .collect()
        }

        fn draw_view(terminal: &mut Terminal<TestBackend>) {
            let cols = terminal.size().expect("size").width;
            let lines = box_lines(cols, MIN_ROWS);
            terminal
                .draw(|f| {
                    f.render_widget(Paragraph::new(Text::from(lines)), f.area());
                })
                .expect("draw");
        }

        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(MIN_ROWS),
            },
        )
        .expect("terminal");

        draw_view(&mut terminal);

        // Shrink the terminal, then re-anchor (the load-bearing call the fix runs
        // inside the EventStream-down window).
        // Narrower than 80, but wide enough for the idle header to fit: a
        // header that overflows has no closing corner to look for.
        terminal.backend_mut().resize(56, 24);
        reanchor_terminal(&mut terminal);
        draw_view(&mut terminal);

        let buf = terminal.backend().buffer();
        // Buffers re-anchored to the new width, not stale 80.
        assert_eq!(buf.area.width, 56, "viewport width should track resize");

        // The box reflowed to the new width: the rounded corners pin to the
        // last column of the header (top-right) and footer (bottom-right) rows.
        let last_col = buf.area.right() - 1;
        let header_y = (buf.area.top()..buf.area.bottom())
            .find(|&y| row_string(buf, y).trim_start().starts_with('╭'))
            .expect("header row");
        let footer_y = (buf.area.top()..buf.area.bottom())
            .find(|&y| row_string(buf, y).trim_start().starts_with('╰'))
            .expect("footer row");
        assert_eq!(
            buf.cell(Position::new(last_col, header_y))
                .map(|c| c.symbol()),
            Some("╮"),
            "header should close at the new last column: {:?}",
            row_string(buf, header_y)
        );
        assert_eq!(
            buf.cell(Position::new(last_col, footer_y))
                .map(|c| c.symbol()),
            Some("╯"),
            "footer should close at the new last column: {:?}",
            row_string(buf, footer_y)
        );
    }

    /// The fewest rows [`box_lines`] draws a box in: top, one body row, bottom,
    /// and a row under it.
    const MIN_ROWS: u16 = 4;

    /// A view's rows for a terminal height, for the re-anchor tests: a third of
    /// the terminal, at least [`MIN_ROWS`], so 24 and 60 rows differ.
    fn rows_for_height(term_height: u16) -> u16 {
        (term_height / 3).max(MIN_ROWS)
    }

    /// A `cols` × `rows` box: `╭─…─╮`, `│ … │` rows, `╰─…─╯`. A stand-in for an
    /// inline view taller than one row, every row of it painted, its right
    /// edge on the last column.
    fn box_lines(cols: u16, rows: u16) -> Vec<ratatui::text::Line<'static>> {
        let inner = usize::from(cols.saturating_sub(2));
        let edge = |l: char, fill: char, r: char| {
            let mut s = String::with_capacity((inner + 2) * 3);
            s.push(l);
            s.extend(std::iter::repeat_n(fill, inner));
            s.push(r);
            ratatui::text::Line::from(s)
        };
        (0..rows)
            .map(|i| match i {
                0 => edge('╭', '─', '╮'),
                i if i + 1 == rows => edge('╰', '─', '╯'),
                _ => edge('│', ' ', '│'),
            })
            .collect()
    }

    /// Rows carrying at least one non-blank cell. The box is the only thing ever
    /// drawn in these tests, so this is "where the box is" — and, more usefully,
    /// "where anything the box left behind is".
    fn painted_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
        (buf.area.top()..buf.area.bottom())
            .filter(|&y| {
                (buf.area.left()..buf.area.right()).any(|x| !buf[(x, y)].symbol().trim().is_empty())
            })
            .collect()
    }

    /// An inline terminal anchored below `origin_row` with a progress box already
    /// drawn into it, i.e. the state a live run is in when SIGWINCH arrives.
    fn live_inline_terminal(
        width: u16,
        height: u16,
        origin_row: u16,
        rows: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::layout::Position;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let mut backend = TestBackend::new(width, height);
        // Anchor away from row 0 so "cursor is at the viewport origin" is a real
        // assertion and not accidentally satisfied by any cursor-home.
        backend
            .set_cursor_position(Position::new(0, origin_row))
            .expect("park cursor");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(rows),
            },
        )
        .expect("terminal");

        let lines = box_lines(width, rows);
        terminal
            .draw(|f| f.render_widget(Paragraph::new(Text::from(lines)), f.area()))
            .expect("draw");
        terminal
    }

    /// Regression: a resize that changes the viewport row count rebuilds the
    /// inline terminal, and `Terminal::with_options` anchors the new viewport at
    /// wherever the cursor happens to be. `autoresize` leaves that cursor at the
    /// *bottom* of the live box, so rebuilding straight after it walks the box a
    /// box-height down the screen on every resize and strands the rows it used to
    /// occupy as leftover artifacts. The path must collapse the viewport first,
    /// parking the cursor back at the box's origin.
    ///
    /// The discriminating observable is *where the rebuilt box lands*, asserted
    /// against the origin captured before the rebuild: `autoresize` has already
    /// erased the old region, so a skipped collapse does not leave glyphs behind —
    /// it silently re-anchors the box a box-height lower (measured: origin 5 → 12
    /// on one resize) and leaves the rows above it as a blank gap that grows with
    /// every subsequent resize.
    #[test]
    fn resize_rebuild_anchors_at_the_old_origin_not_the_live_cursor() {
        use super::reanchor_inline;
        use ratatui::Terminal;
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::layout::Position;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;
        use ratatui::{TerminalOptions, Viewport};

        const WIDTH: u16 = 80;
        const ORIGIN: u16 = 5;

        let mut rows = rows_for_height(24);
        let mut terminal = live_inline_terminal(WIDTH, 24, ORIGIN, rows);

        let box_before: Vec<u16> = painted_rows(terminal.backend().buffer());
        assert!(
            box_before.first().is_some_and(|&y| y >= ORIGIN),
            "precondition: the box is anchored below row 0, got {box_before:?}"
        );
        // The draw left the cursor at the last cell it wrote — inside the box, not
        // at its origin. That is what a naive rebuild would anchor to.
        assert_ne!(
            terminal
                .backend_mut()
                .get_cursor_position()
                .expect("cursor"),
            Position::new(0, terminal.get_frame().area().y),
            "precondition: a live box leaves the cursor away from the viewport origin"
        );

        // Grow the terminal (same width, so ratatui's horizontal-shrink clear-all
        // doesn't paper over the anchor) into a height whose ~1/3 row count
        // differs — the branch that rebuilds the terminal.
        let new_height = 60;
        let expected_rows = rows_for_height(new_height);
        assert_ne!(
            expected_rows, rows,
            "test must exercise the row-count-changed rebuild branch"
        );
        terminal.backend_mut().resize(WIDTH, new_height);

        // `autoresize` (the first thing `reanchor_inline` does) recomputes the
        // inline anchor; that is the origin the rebuilt viewport has to reuse.
        let origin_before = terminal.get_frame().area().y;
        reanchor_inline(
            &mut terminal,
            &mut rows,
            rows_for_height,
            |desired, collapsed| {
                // Carry the backend across, exactly as production does — the rebuild
                // re-wraps the same stderr, it does not get a fresh screen.
                Terminal::with_options(
                    collapsed.backend().clone(),
                    TerminalOptions {
                        viewport: Viewport::Inline(desired),
                    },
                )
                .ok()
            },
        );
        assert_eq!(rows, expected_rows, "viewport adopts the new row count");
        assert_eq!(
            terminal.get_frame().area().y,
            origin_before,
            "the rebuilt viewport must reuse the old box's rows; anchoring on the \
             live cursor walks it down the screen instead"
        );

        let lines = box_lines(WIDTH, rows);
        terminal
            .draw(|f| f.render_widget(Paragraph::new(Text::from(lines)), f.area()))
            .expect("redraw");
        assert_eq!(
            painted_rows(terminal.backend().buffer()),
            (origin_before..origin_before + rows).collect::<Vec<_>>(),
            "exactly one box, starting where the old one did"
        );
    }

    /// The common resize: a width-only drag. The row count is unchanged, so the
    /// terminal must not be rebuilt at all — `autoresize` alone re-anchors and
    /// resets the back buffer, and the box reflows to the new width in place.
    #[test]
    fn width_only_resize_reflows_without_rebuilding() {
        use super::reanchor_inline;
        use ratatui::backend::Backend;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;

        let start_rows = rows_for_height(24);
        let mut rows = start_rows;
        let mut terminal = live_inline_terminal(80, 24, 5, rows);
        // Same height, so `rows_for_height` is unchanged — only the width moved.
        terminal.backend_mut().resize(50, 24);

        let mut rebuilds = 0;
        reanchor_inline(
            &mut terminal,
            &mut rows,
            rows_for_height,
            |_desired, _collapsed| {
                rebuilds += 1;
                None
            },
        );
        assert_eq!(rebuilds, 0, "no row-count change, no rebuild");
        assert_eq!(rows, start_rows);

        let lines = box_lines(50, rows);
        terminal
            .draw(|f| f.render_widget(Paragraph::new(Text::from(lines)), f.area()))
            .expect("redraw");
        let origin = terminal.get_frame().area().y;
        assert_eq!(
            painted_rows(terminal.backend().buffer()),
            (origin..origin + rows).collect::<Vec<_>>(),
            "the reflowed box is the only thing on screen — no stale wide-row tails"
        );
    }

    /// `Terminal::with_options` re-queries the cursor and can fail on a terminal
    /// that never answers. The old viewport has already been collapsed by then, so
    /// the kept terminal must still repaint cleanly rather than sit blank.
    #[test]
    fn failed_rebuild_keeps_the_old_terminal_usable() {
        use super::reanchor_inline;
        use ratatui::backend::Backend;
        use ratatui::text::Text;
        use ratatui::widgets::Paragraph;

        const WIDTH: u16 = 80;
        let start_rows = rows_for_height(24);
        let mut rows = start_rows;
        let mut terminal = live_inline_terminal(WIDTH, 24, 5, rows);
        terminal.backend_mut().resize(WIDTH, 60);
        assert_ne!(rows_for_height(60), rows, "must hit the rebuild branch");

        let origin_before = terminal.get_frame().area().y;
        reanchor_inline(
            &mut terminal,
            &mut rows,
            rows_for_height,
            |_desired, _collapsed| None,
        );
        assert_eq!(rows, start_rows, "row count only moves on a success");
        assert_eq!(
            terminal.get_frame().area().y,
            origin_before,
            "a failed rebuild leaves the viewport where the collapse parked it"
        );

        let lines = box_lines(WIDTH, rows);
        terminal
            .draw(|f| f.render_widget(Paragraph::new(Text::from(lines)), f.area()))
            .expect("redraw");
        let origin = terminal.get_frame().area().y;
        assert_eq!(
            painted_rows(terminal.backend().buffer()),
            (origin..origin + rows).collect::<Vec<_>>(),
            "the collapsed viewport repaints in full after a failed rebuild"
        );
    }

    #[test]
    fn rows_needed_handles_boundaries() {
        assert_eq!(rows_needed(0, 80), 1);
        assert_eq!(rows_needed(1, 80), 1);
        assert_eq!(rows_needed(80, 80), 1);
        assert_eq!(rows_needed(81, 80), 2);
        assert_eq!(rows_needed(160, 80), 2);
        assert_eq!(rows_needed(161, 80), 3);
    }

    #[test]
    fn rows_needed_handles_zero_cols() {
        // cols clamped to 1
        assert_eq!(rows_needed(5, 0), 5);
    }

    #[test]
    fn ansi_escapes_do_not_inflate_width() {
        let bytes = b"\x1b[31mfoo\x1b[0m".to_vec();
        let text = bytes.into_text().expect("parse ansi");
        let total: usize = text.lines.iter().map(|l| l.width()).sum();
        assert_eq!(total, 3, "width should count visible chars only");
    }

    /// A budgeted drain (the tick path) must never throw a message away when it
    /// stops early — it must leave it queued in `rx` for the next call. The tick
    /// path is the only call site where budgeting is safe *because* it reuses the
    /// same `rx` across calls (see `drain_logs_to_terminal`'s docs); this freezes
    /// that contract.
    #[test]
    fn tick_budget_defers_excess_messages_instead_of_dropping_them() {
        use super::{SinkLine, drain_logs_to_terminal};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::{TerminalOptions, Viewport};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SinkLine>();
        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("terminal");

        for i in 0..3 {
            tx.send(SinkLine {
                seq: i + 1,
                bytes: format!("line {i}\n").into_bytes(),
            })
            .expect("send");
        }

        // The deadline is captured once, before the loop, as `Instant::now() +
        // Duration::ZERO` — i.e. already in the past the moment any real work
        // happens after it. The budget is only checked after a message has been
        // fully rendered (`terminal.insert_before`, never instantaneous), so the
        // very first check is guaranteed to trip: this deterministically drains
        // exactly one message and stops, on any of the three supported targets.
        drain_logs_to_terminal(&mut terminal, &mut rx, 80, Some(Duration::ZERO), 0);

        let mut remaining = Vec::new();
        while let Ok(line) = rx.try_recv() {
            remaining.push(line.bytes);
        }
        assert_eq!(
            remaining,
            vec![b"line 1\n".to_vec(), b"line 2\n".to_vec()],
            "a budgeted drain must leave the rest of the burst queued for the \
             next tick, not discard it — the tick path reuses the same `rx`, so \
             nothing is lost, only deferred"
        );
    }

    /// A burst that outlasts a single budgeted drain must still fully drain once
    /// enough ticks have run — deferral has to actually converge, not just hold
    /// for one call. Each of the `messages.len()` calls below is budgeted to
    /// process exactly one message (same zero-budget reasoning as the test
    /// above), mirroring `messages.len()` consecutive render ticks against the
    /// same `rx`.
    #[test]
    fn tick_budget_converges_across_multiple_ticks_without_losing_or_reordering() {
        use super::{SinkLine, drain_logs_to_terminal};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::{TerminalOptions, Viewport};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SinkLine>();
        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("terminal");

        let messages: Vec<Vec<u8>> = (0..5).map(|i| format!("line {i}\n").into_bytes()).collect();
        for (seq, m) in (1..).zip(&messages) {
            tx.send(SinkLine {
                seq,
                bytes: m.clone(),
            })
            .expect("send");
        }

        // One zero-budget call per message, exactly mirroring one render tick
        // each — plus one extra call to prove the channel is empty afterward,
        // not just down to one leftover message.
        for _ in 0..messages.len() {
            drain_logs_to_terminal(&mut terminal, &mut rx, 80, Some(Duration::ZERO), 0);
        }

        assert!(
            rx.try_recv().is_err(),
            "a burst spanning multiple ticks must fully drain once enough \
             budgeted calls have run — deferral must converge, not stall"
        );
        // `drain_logs_to_terminal` unconditionally renders a message via
        // `terminal.insert_before` before it can be taken off `rx` (see its
        // body) — so with `rx` empty and no panic, all `messages.len()` sends
        // were rendered exactly once across the loop above, none silently
        // skipped.
    }

    /// Regression for the pause-path loss: `LogSink::switch_to_buffered` mints a
    /// brand-new channel every time the TUI resumes, so the pause handler cannot
    /// budget its drain like the tick path does — anything left in the old `rx`
    /// when resume replaces it is gone for good, not merely delayed to a future
    /// call. Worse, a line written in the gap between the pause-time drain and
    /// the sink flipping to direct writes still lands in that same soon-to-be-
    /// replaced `rx`, since the sink hasn't flipped yet.
    ///
    /// This drives the actual `LogSink`/`MakeLogSink` types and the real
    /// `flip_sink_to_direct_draining_the_gap` function `interactive::run`'s
    /// `Control::Pause` arm calls — not a hand-mirrored copy — so it exercises
    /// the production fix directly. Only the surrounding pause/resume cycle
    /// (which needs a real terminal) is left out, for the same reason the resize
    /// test above does: not honestly unit-testable without a PTY.
    ///
    /// Verified red without the fix: with the `drain_logs_to_stderr(rx)` call
    /// removed from `flip_sink_to_direct_draining_the_gap`, the gap message stays
    /// in `rx` and the final assertion fails — exactly what would happen on a
    /// real resume, where `switch_to_buffered` mints a replacement channel and
    /// silently drops whatever the old one still held.
    #[test]
    fn pause_gap_message_is_drained_not_orphaned_before_resume_can_replace_rx() {
        use super::{drain_logs_to_terminal, flip_sink_to_direct_draining_the_gap};
        use crate::tui::log_sink::{LogSink, MakeLogSink};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::{TerminalOptions, Viewport};
        use std::io::Write;
        use tracing_subscriber::fmt::MakeWriter;

        let sink = LogSink::new_direct_with(Default::default());
        let mut rx = sink.switch_to_buffered();
        let mut writer = MakeLogSink::new(sink.clone()).make_writer();

        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("terminal");

        // Steady-state log line, drained the way the pause handler drains before
        // collapsing the viewport.
        writer.write_all(b"before pause\n").expect("write");
        drain_logs_to_terminal(&mut terminal, &mut rx, 80, None, 0);
        assert!(
            rx.try_recv().is_err(),
            "precondition: the steady-state drain leaves nothing behind"
        );

        // The race: a line written in the gap between that drain and the sink
        // flipping to direct writes. It lands in the same `rx` — the sink hasn't
        // flipped yet — exactly like a real writer racing the pause transition.
        writer.write_all(b"during the gap\n").expect("write");

        // The fix under test: the real function `run`'s pause arm calls.
        flip_sink_to_direct_draining_the_gap(&sink, &mut rx, 0);

        assert!(
            rx.try_recv().is_err(),
            "a message written in the pause/switch_to_direct gap must be drained \
             before resume can replace the channel — `LogSink::switch_to_buffered` \
             mints a fresh one on resume, orphaning whatever is left in the old \
             `rx` for good, not deferring it"
        );
    }
}
