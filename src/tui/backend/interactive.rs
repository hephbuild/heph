use std::io::{self, Write};
use std::time::Duration;

use ansi_to_tui::IntoText;
use anyhow::Context;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::prelude::Widget;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::commands::bootstrap::ShutdownTrigger;
use crate::tui::app::{App, AppContext, Control};
use crate::tui::log_sink::LogSink;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);

type StderrTerminal = Terminal<CrosstermBackend<io::Stderr>>;

pub async fn run<A: App + 'static>(
    app: A,
    sink: LogSink,
    shutdown: ShutdownTrigger,
) -> anyhow::Result<A::Output> {
    let label = app.label();
    let mut rx = sink.switch_to_buffered();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();

    enable_raw_mode().context("enabling raw mode")?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stderr()),
        TerminalOptions {
            viewport: Viewport::Inline(1),
        },
    )
    .context("building inline terminal")?;
    terminal.autoresize()?;

    let mut events = EventStream::new();
    let suppression = shutdown.suppression();

    let ctx = AppContext::with_control(sink.clone(), control_tx);
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

    let result: anyhow::Result<A::Output> = loop {
        tokio::select! {
            res = &mut app_handle => {
                break match res {
                    Ok(inner) => inner,
                    Err(join_err) if join_err.is_panic() => {
                        std::panic::resume_unwind(join_err.into_panic())
                    }
                    Err(join_err) => Err(anyhow::Error::new(join_err).context("app task")),
                };
            }
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(Control::Pause(ack)) => {
                        if !paused {
                            // Suppress shutdown trigger before releasing raw mode so
                            // a Ctrl+C delivered to the cooked-mode prompt can't
                            // race past us and cancel engine work.
                            suppression.set(true);
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
                        rx = sink.switch_to_buffered();
                        cols = terminal_cols(&terminal);
                        paused = false;
                        suppression.set(false);
                    }
                    Some(Control::Resume) => {}
                    None => {}
                }
            }
            maybe_evt = events.next(), if !paused => {
                match maybe_evt {
                    Some(Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers,
                        kind: KeyEventKind::Press,
                        ..
                    }))) if modifiers.contains(KeyModifiers::CONTROL) => {
                        shutdown.trigger();
                    }
                    Some(Ok(Event::Resize(w, _))) => {
                        cols = w.max(1);
                    }
                    _ => {}
                }
            }
            _ = ticker.tick(), if !paused => {
                drain_logs_to_terminal(&mut terminal, &mut rx, cols);
                spinner_idx = (spinner_idx + 1) % SPINNER_FRAMES.len();
                let frame = SPINNER_FRAMES.get(spinner_idx).copied().unwrap_or("");
                let line = format!("{frame} {label}");
                drop(terminal.draw(|f| {
                    let area = f.area();
                    f.render_widget(Paragraph::new(line), area);
                }));
            }
        }
    };

    if !paused {
        drain_logs_to_terminal(&mut terminal, &mut rx, cols);
        drop(terminal.clear());
        drop(terminal.show_cursor());
        drop(disable_raw_mode());
    }
    sink.switch_to_direct();
    drain_logs_to_stderr(&mut rx);

    result
}

fn terminal_cols(terminal: &StderrTerminal) -> u16 {
    terminal.size().map(|r| r.width).unwrap_or(80).max(1)
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
