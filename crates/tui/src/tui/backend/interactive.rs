use std::io::{self, Write};
use std::time::Duration;

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
                            drain_logs_to_terminal(&mut terminal, &mut rx, cols);
                            drop(terminal.clear());
                            drop(terminal.show_cursor());
                            drop(disable_raw_mode());
                            sink.switch_to_direct();
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
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('q'),
                        kind: KeyEventKind::Press,
                        ..
                    }))) if held && !view.is_searching() => {
                        quit = true;
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
                    // Esc clears the filter whether mid-type or already confirmed.
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Esc,
                        kind: KeyEventKind::Press,
                        ..
                    }))) => view.search_cancel(),
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
                    reanchor_after_resize(&mut terminal, &mut events, &mut cols, &mut rows);
                    needs_resize = false;
                }
                drain_logs_to_terminal(&mut terminal, &mut rx, cols);
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

    if !paused {
        // Flush any still-buffered build-event logs into the terminal scrollback
        // *above* the viewport. This must stay on the terminal path
        // (`insert_before`): it wraps to the viewport width and skips blank lines.
        // The post-teardown `drain_logs_to_stderr` fallback writes raw bytes, so
        // letting empties fall through to it dumps stray newlines after the box.
        drain_logs_to_terminal(&mut terminal, &mut rx, cols);
        // Render one final frame and capture its viewport origin in the *same*
        // draw, so the anchor matches exactly where the box sits on screen — the
        // drain above may have scrolled the viewport via `insert_before`, so a
        // stale origin would be off by the inserted line count. `Frame::area().y`
        // is ratatui's own anchor for the inline region — no cursor DSR query, so
        // nothing can race crossterm's reader.
        let frame = SPINNER_FRAMES.get(spinner_idx).copied().unwrap_or("");
        let lines = view.render(frame, now_unix_ms(), cols, rows);
        let mut anchor_row: u16 = 0;
        drop(terminal.draw(|f| {
            let area = f.area();
            anchor_row = area.y;
            f.render_widget(Paragraph::new(Text::from(lines)), area);
        }));
        // Collapse the inline viewport and leave the cursor at its origin so the
        // final summary (printed below) lands where the box started. We do this
        // by hand from `anchor_row` rather than via `terminal.clear()`, which
        // would issue a cursor DSR query (deadlocking crossterm's reader vs the
        // EventStream) and restore the live bottom-of-box cursor.
        let backend = terminal.backend_mut();
        drop(backend.set_cursor_position(Position {
            x: 0,
            y: anchor_row,
        }));
        drop(backend.clear_region(ClearType::AfterCursor));
        drop(backend.show_cursor());
        drop(Backend::flush(backend));
        drop(disable_raw_mode());
    }
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
) {
    *events = None;
    let term_height = terminal.size().map(|r| r.height).unwrap_or(24).max(1);
    let desired = crate::tui::progress::rows_for_height(term_height);
    if desired != *rows {
        // Row count changed: rebuild the inline terminal so the viewport reserves
        // the new height. `with_options` re-queries the cursor and re-anchors the
        // viewport, same as the pause/resume rebuild.
        if let Ok(new_term) = Terminal::with_options(
            StderrBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Inline(desired),
            },
        ) {
            *terminal = new_term;
            *rows = desired;
        } else {
            reanchor_terminal(terminal);
        }
    } else {
        reanchor_terminal(terminal);
    }
    *events = Some(EventStream::new());
    // The backend size ratatui actually re-anchored to is the source of truth.
    *cols = terminal_cols(terminal);
}

fn drain_logs_to_terminal(
    terminal: &mut StderrTerminal,
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cols: u16,
) {
    let cols = cols.max(1);
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
}
