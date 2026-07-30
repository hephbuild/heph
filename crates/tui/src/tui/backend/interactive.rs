use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Context;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::prelude::Widget;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::tui::app::{App, AppContext, Control, TUIAppView};
use crate::tui::log_sink::LogSink;
use crate::tui::progress::HSCROLL_STEP;
use crate::tui::stderr_backend::StderrBackend;
use hcore::events::{EventReceiver, now_unix_ms};
use hcore::shutdown::ShutdownTrigger;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);
// Wall-clock cap on how long a single tick's log drain may run — bounds
// redraw latency during a burst without ever dropping a line: the same `rx`
// persists across ticks, so anything left over just renders on the next one.
// Well under `TICK` so a capped drain still leaves room for the redraw.
const LOG_DRAIN_TICK_BUDGET: Duration = Duration::from_millis(20);

type StderrTerminal = Terminal<StderrBackend>;

pub async fn run<A: App + 'static>(
    app: A,
    sink: LogSink,
    shutdown: ShutdownTrigger,
) -> anyhow::Result<A::Output> {
    // The app owns its view: aggregation + rendering. It lives on the loop
    // stack so it survives pause/resume (only the terminal is rebuilt across a
    // pause cycle, not the view's aggregated state).
    let mut view = app.tui_view();
    // Size the inline viewport to ~1/3 of the terminal height. Queried before the
    // viewport is built (so the first frame is already sized); the column count is
    // re-derived from the backend below.
    let term_height = crossterm::terminal::size()
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
    let mut build_events: Option<EventReceiver> = Some(event_rx);

    record_raw_mode_restore();
    enable_raw_mode().context("enabling raw mode")?;
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

    let mut events: Option<EventStream> = Some(EventStream::new());
    let suppression = shutdown.suppression();

    // Shared with the app's request state: the engine registers fire-and-forget
    // sandbox cleanups against this counter. We keep rendering until the app
    // future resolves AND this drains, so the run visibly stays up while
    // background cleanups finish during exit (and the process doesn't tear the
    // cleaner thread out mid-rmdir).
    let bg_pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
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
    let mut app_handle = tokio::spawn(app.run(ctx));

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut spinner_idx: usize = 0;
    let mut paused = false;
    let mut cols = terminal_cols(&terminal);
    // Set when a resize event arrives; handled (coalesced) at the next tick so a
    // drag-resize burst triggers at most one re-anchor per tick instead of a
    // blocking DSR query per event.
    let mut needs_resize = false;

    // Holds the app's result once its future resolves. We don't break the loop
    // here: the TUI stays up, rendering, until background cleanups drain too.
    let mut app_result: Option<anyhow::Result<A::Output>> = None;
    // Latched once the finished run is held open on a non-main view: the viewport
    // keeps rendering until the user quits explicitly (`q` / Ctrl-C).
    let mut held = false;
    // Set by `q` / Ctrl-C to break out of a held viewport.
    let mut quit = false;
    let result: anyhow::Result<A::Output> = loop {
        // App finished and the background queue is empty — decide whether to exit.
        if let Some(r) = app_result.take() {
            if bg_pending.load(std::sync::atomic::Ordering::Acquire) == 0 {
                if held {
                    // Held open after finish — exit only on explicit quit.
                    if quit {
                        break r;
                    }
                    app_result = Some(r);
                } else if view.hold_after_finish() {
                    // User navigated off the main view: hold the viewport up and
                    // surface a "press q to quit" notice instead of auto-exiting.
                    held = true;
                    view.set_finished();
                    app_result = Some(r);
                } else {
                    break r;
                }
            } else {
                // Background cleanups still draining — keep the result, keep rendering.
                app_result = Some(r);
            }
        }
        tokio::select! {
            res = &mut app_handle, if app_result.is_none() => {
                app_result = Some(match res {
                    Ok(inner) => inner,
                    Err(join_err) if join_err.is_panic() => {
                        std::panic::resume_unwind(join_err.into_panic())
                    }
                    Err(join_err) => Err(anyhow::Error::new(join_err).context("app task")),
                });
            }
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(Control::Pause(ack)) => {
                        if !paused {
                            // Suppress shutdown trigger before releasing raw mode so
                            // a Ctrl+C delivered to the cooked-mode prompt can't
                            // race past us and cancel engine work.
                            suppression.set(true);
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
                            drain_logs_to_terminal(&mut terminal, &mut rx, cols, None);
                            // Collapse to the viewport origin (not `terminal.clear()`,
                            // which restores the live bottom-of-box cursor): the
                            // cooked-mode stdout write below must land at the box's
                            // top so the resume re-anchor reuses the cleared rows
                            // instead of stranding them as a blank gap.
                            collapse_inline_viewport(&mut terminal);
                            drop(terminal.show_cursor());
                            drop(disable_raw_mode());
                            flip_sink_to_direct_draining_the_gap(&sink, &mut rx);
                            paused = true;
                        }
                        if ack.send(()).is_err() {
                            // Receiver dropped — app no longer waiting on pause ack.
                        }
                    }
                    Some(Control::Resume) if paused => {
                        drop(enable_raw_mode());
                        // Cooked-mode writes during pause moved the cursor by an
                        // unknown amount; ratatui's internal state (viewport_area,
                        // last_known_cursor_pos, both buffers) is stale. Rebuild
                        // the terminal so `with_options`+`compute_inline_size`
                        // re-queries the cursor and positions the viewport below
                        // the printed output instead of clobbering it.
                        if let Ok(new_term) = Terminal::with_options(
                            StderrBackend::new(io::stderr()),
                            TerminalOptions {
                                viewport: Viewport::Inline(rows),
                            },
                        ) {
                            terminal = new_term;
                        }
                        events = Some(EventStream::new());
                        rx = sink.switch_to_buffered();
                        cols = terminal_cols(&terminal);
                        paused = false;
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
                match maybe_evt {
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if modifiers.contains(KeyModifiers::CONTROL) => {
                        shutdown.trigger();
                        // Also quits a viewport held open after the run finished.
                        quit = true;
                    }
                    // `q` off the main view returns to it (like Esc); on the main
                    // view it quits a held (finished) viewport. Ignored mid-search
                    // (captured as query input below).
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('q'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if !view.is_searching() => {
                        if view.is_on_main_view() {
                            if held {
                                quit = true;
                            }
                        } else {
                            view.back_to_main();
                        }
                    }
                    // Approval prompt active: `y`/`n` resolve the gated target and
                    // Enter expands/collapses its notice. These precede the normal
                    // shortcuts so a gated run is resolvable without other keys
                    // (e.g. `n`/`a`) being intercepted first.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('y'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.approval_active() => view.approval_respond(true),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('n'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.approval_active() => view.approval_respond(false),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if view.approval_active() && !view.is_searching() => {
                        view.approval_toggle_notice()
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
                        code: KeyCode::Left,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.hscroll(-(HSCROLL_STEP as i32)),
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Right,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.hscroll(HSCROLL_STEP as i32),
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
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('a'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.toggle_scope(),
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
                    Some(Ok(Event::Resize(w, _))) => {
                        // Cheap: just record. The terminal re-anchor (which does a
                        // DSR cursor query that must not race the EventStream) is
                        // deferred to the tick arm below.
                        cols = w.max(1);
                        needs_resize = true;
                    }
                    _ => {}
                }
            }
            maybe_build_evt = async {
                match build_events.as_mut() {
                    Some(r) => r.recv().await,
                    None => std::future::pending().await,
                }
            }, if !paused => {
                match maybe_build_evt {
                    // No per-event redraw: fold into the view here, the 80ms
                    // ticker repaints the pinned progress block.
                    Some(e) => view.apply(&e),
                    // Sender dropped (request finished) — stop polling.
                    None => build_events = None,
                }
            }
            _ = ticker.tick(), if !paused => {
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
                // renders on the next tick — deferred, never dropped.
                drain_logs_to_terminal(&mut terminal, &mut rx, cols, Some(LOG_DRAIN_TICK_BUDGET));
                spinner_idx = (spinner_idx + 1) % SPINNER_FRAMES.len();
                let frame = SPINNER_FRAMES.get(spinner_idx).copied().unwrap_or("");
                let lines = view.render(frame, now_unix_ms(), cols, rows);
                drop(terminal.draw(|f| {
                    let area = f.area();
                    f.render_widget(Paragraph::new(Text::from(lines)), area);
                }));
            }
        }
    };

    if paused {
        // The run finished while a `BufferedStdout` flush had the TUI paused: the
        // pause already tore the viewport down (cleared + cooked mode) and wrote
        // straight to stdout, so the live cursor now sits just below that output.
        // Rebuild the terminal so its inline viewport re-anchors there (a DSR
        // query with no `EventStream` alive — the pause dropped it), then fall
        // through to the same collapse. Without this the summary would print at
        // the stale paused cursor, stranding the reserved viewport rows as a
        // blank gap above it.
        //
        // Use a 1-row viewport, not the full box height: we only render the
        // one-line summary from here on, and a tall viewport whose cursor sits at
        // the bottom of the screen (after a long stdout dump) makes
        // `compute_inline_size` scroll a box-height of blank rows in to reserve
        // space — exactly the gap we're avoiding.
        drop(enable_raw_mode());
        if let Ok(rebuilt) = Terminal::with_options(
            StderrBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Inline(1),
            },
        ) {
            terminal = rebuilt;
        }
        cols = terminal_cols(&terminal);
    }

    // Flush any still-buffered build-event logs into the terminal scrollback
    // *above* the viewport. This must stay on the terminal path (`insert_before`):
    // it wraps to the viewport width and skips blank lines. The post-teardown
    // `drain_logs_to_stderr` fallback writes raw bytes, so letting empties fall
    // through to it dumps stray newlines after the box.
    drain_logs_to_terminal(&mut terminal, &mut rx, cols, None);
    // Collapse the viewport to its origin so the final summary (printed below)
    // lands where the box started, not below it.
    collapse_inline_viewport(&mut terminal);
    {
        let backend = terminal.backend_mut();
        drop(backend.show_cursor());
        drop(Backend::flush(backend));
    }
    drop(disable_raw_mode());
    hcore::shutdown::clear_terminal_restore();
    sink.switch_to_direct();
    drain_logs_to_stderr(&mut rx);

    // The app completing breaks the loop, but build events emitted just before
    // it returned may still be buffered (the sender closes only once the
    // request state drops). Drain them so the final summary reflects the full
    // stream rather than the snapshot at the last tick.
    if let Some(r) = build_events.as_mut() {
        while let Ok(e) = r.try_recv() {
            view.apply(&e);
        }
    }

    // Persistent final summary, printed straight to stderr below the
    // torn-down inline viewport (interactive mode only).
    view.last_render();

    result
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
fn drain_logs_to_terminal<B: Backend>(
    terminal: &mut Terminal<B>,
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cols: u16,
    budget: Option<Duration>,
) {
    let cols = cols.max(1);
    let deadline = budget.map(|d| Instant::now() + d);
    while let Ok(bytes) = rx.try_recv() {
        let text = bytes
            .into_text()
            .unwrap_or_else(|_| Text::raw(String::from_utf8_lossy(&bytes).into_owned()));
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
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
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

fn drain_logs_to_stderr(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) {
    let mut stderr = io::stderr().lock();
    while let Ok(bytes) = rx.try_recv() {
        drop(stderr.write_all(&bytes));
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
fn flip_sink_to_direct_draining_the_gap(sink: &LogSink, rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) {
    sink.switch_to_direct();
    drain_logs_to_stderr(rx);
}

#[cfg(test)]
mod tests {
    use super::{IntoText, TICK, rows_needed};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

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

    /// The exit loop must not break the moment the app future resolves: it keeps
    /// rendering until the shared background-cleanup counter drains to zero. This
    /// mirrors the break condition in `run` (app done AND bg_pending == 0) without
    /// a real terminal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn loop_stays_open_until_bg_pending_drains() {
        use std::sync::atomic::AtomicUsize;
        // App finishes immediately, but leaves 1 unit of background work that a
        // separate task clears after a delay.
        let bg_pending = Arc::new(AtomicUsize::new(1));
        let app = tokio::spawn(async { 7u8 });
        tokio::pin!(app);

        let drainer = {
            let bg = Arc::clone(&bg_pending);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(120)).await;
                bg.store(0, Ordering::Release);
            })
        };

        let mut app_result: Option<u8> = None;
        let mut ticks_after_app = 0usize;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let result = loop {
            if let Some(r) = app_result.take() {
                if bg_pending.load(Ordering::Acquire) == 0 {
                    break r;
                }
                app_result = Some(r);
            }
            tokio::select! {
                res = &mut app, if app_result.is_none() => {
                    app_result = Some(res.expect("app task"));
                }
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
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::{MIN_PROGRESS_ROWS, TuiProgressView};
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

        fn draw_view(terminal: &mut Terminal<TestBackend>, view: &TuiProgressView) {
            let cols = terminal.size().expect("size").width;
            let lines = view.render("⠋", 10_000, cols, MIN_PROGRESS_ROWS);
            terminal
                .draw(|f| {
                    f.render_widget(Paragraph::new(Text::from(lines)), f.area());
                })
                .expect("draw");
        }

        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(MIN_PROGRESS_ROWS),
            },
        )
        .expect("terminal");

        let view = TuiProgressView::new("Running //a:b");
        draw_view(&mut terminal, &view);

        // Shrink the terminal, then re-anchor (the load-bearing call the fix runs
        // inside the EventStream-down window).
        terminal.backend_mut().resize(40, 24);
        reanchor_terminal(&mut terminal);
        draw_view(&mut terminal, &view);

        let buf = terminal.backend().buffer();
        // Buffers re-anchored to the new width, not stale 80.
        assert_eq!(buf.area.width, 40, "viewport width should track resize");

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
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::TuiProgressView;
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

        let view = TuiProgressView::new("Running //a:b");
        let lines = view.render("⠋", 10_000, width, rows);
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
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::{TuiProgressView, rows_for_height};
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

        let view = TuiProgressView::new("Running //a:b");
        let lines = view.render("⠋", 10_000, WIDTH, rows);
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
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::{TuiProgressView, rows_for_height};
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

        let view = TuiProgressView::new("Running //a:b");
        let lines = view.render("⠋", 10_000, 50, rows);
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
        use crate::tui::app::TUIAppView;
        use crate::tui::progress::{TuiProgressView, rows_for_height};
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

        let view = TuiProgressView::new("Running //a:b");
        let lines = view.render("⠋", 10_000, WIDTH, rows);
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
        use super::drain_logs_to_terminal;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::{TerminalOptions, Viewport};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("terminal");

        for i in 0..3 {
            tx.send(format!("line {i}\n").into_bytes()).expect("send");
        }

        // The deadline is captured once, before the loop, as `Instant::now() +
        // Duration::ZERO` — i.e. already in the past the moment any real work
        // happens after it. The budget is only checked after a message has been
        // fully rendered (`terminal.insert_before`, never instantaneous), so the
        // very first check is guaranteed to trip: this deterministically drains
        // exactly one message and stops, on any of the three supported targets.
        drain_logs_to_terminal(&mut terminal, &mut rx, 80, Some(Duration::ZERO));

        let mut remaining = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            remaining.push(bytes);
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
        use super::drain_logs_to_terminal;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::{TerminalOptions, Viewport};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let mut terminal = Terminal::with_options(
            TestBackend::new(80, 24),
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("terminal");

        let messages: Vec<Vec<u8>> = (0..5).map(|i| format!("line {i}\n").into_bytes()).collect();
        for m in &messages {
            tx.send(m.clone()).expect("send");
        }

        // One zero-budget call per message, exactly mirroring one render tick
        // each — plus one extra call to prove the channel is empty afterward,
        // not just down to one leftover message.
        for _ in 0..messages.len() {
            drain_logs_to_terminal(&mut terminal, &mut rx, 80, Some(Duration::ZERO));
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

        let sink = LogSink::new_direct();
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
        drain_logs_to_terminal(&mut terminal, &mut rx, 80, None);
        assert!(
            rx.try_recv().is_err(),
            "precondition: the steady-state drain leaves nothing behind"
        );

        // The race: a line written in the gap between that drain and the sink
        // flipping to direct writes. It lands in the same `rx` — the sink hasn't
        // flipped yet — exactly like a real writer racing the pause transition.
        writer.write_all(b"during the gap\n").expect("write");

        // The fix under test: the real function `run`'s pause arm calls.
        flip_sink_to_direct_draining_the_gap(&sink, &mut rx);

        assert!(
            rx.try_recv().is_err(),
            "a message written in the pause/switch_to_direct gap must be drained \
             before resume can replace the channel — `LogSink::switch_to_buffered` \
             mints a fresh one on resume, orphaning whatever is left in the old \
             `rx` for good, not deferring it"
        );
    }
}
