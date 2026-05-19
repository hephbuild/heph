use std::io::{self, Write};
use std::time::Duration;

use anyhow::Context;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::prelude::Widget;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::tui::app::{App, AppContext, Control};
use crate::tui::log_sink::LogSink;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);

type StderrTerminal = Terminal<CrosstermBackend<io::Stderr>>;

pub async fn run<A: App>(app: A, sink: LogSink) -> anyhow::Result<A::Output> {
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

    let ctx = AppContext::with_control(sink.clone(), control_tx);
    let app_fut = app.run(ctx);
    tokio::pin!(app_fut);

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut spinner_idx: usize = 0;
    let mut paused = false;

    let result: anyhow::Result<A::Output> = loop {
        tokio::select! {
            biased;
            res = &mut app_fut => break res,
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(Control::Pause(ack)) => {
                        if !paused {
                            drain_logs_to_terminal(&mut terminal, &mut rx);
                            drop(terminal.clear());
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
                        paused = false;
                    }
                    Some(Control::Resume) => {}
                    None => {}
                }
            }
            _ = ticker.tick(), if !paused => {
                drain_logs_to_terminal(&mut terminal, &mut rx);
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
        drain_logs_to_terminal(&mut terminal, &mut rx);
        drop(terminal.clear());
        drop(disable_raw_mode());
    }
    sink.switch_to_direct();
    drain_logs_to_stderr(&mut rx);

    result
}

fn drain_logs_to_terminal(
    terminal: &mut StderrTerminal,
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Ok(bytes) = rx.try_recv() {
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let owned = line.to_string();
            drop(terminal.insert_before(1, |buf: &mut Buffer| {
                let area = buf.area;
                Paragraph::new(owned.clone()).render(area, buf);
            }));
        }
    }
}

fn drain_logs_to_stderr(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) {
    let mut stderr = io::stderr().lock();
    while let Ok(bytes) = rx.try_recv() {
        drop(stderr.write_all(&bytes));
    }
}
