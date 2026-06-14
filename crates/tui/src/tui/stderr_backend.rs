//! ratatui [`Backend`] that wraps [`CrosstermBackend<io::Stderr>`] and
//! intercepts `get_cursor_position`.
//!
//! ratatui's inline viewport anchors itself with an `ESC[6n` cursor query.
//! crossterm's `cursor::position()` writes that to `io::stdout()`, which corrupts
//! a piped stdout (`heph … | cmd`). We use the fork's [`cursor::position_to`] to
//! send it to stderr (the tty the TUI already draws on) while still reading the
//! reply through crossterm's shared input reader (so it demuxes from key events).
//!
//! No caching: this is only ever called when no `EventStream` is alive — at the
//! initial `with_options`, and the resize/resume rebuilds, which tear the stream
//! down first. crossterm's single global reader would deadlock a query made while
//! the stream is actively polling, so callers must keep to those windows. The
//! exit teardown does *not* query the cursor (see `interactive.rs`): it collapses
//! the viewport using the origin reported by `Frame::area()`.

use std::io::{self, BufWriter};
use std::ops::Range;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct StderrBackend {
    inner: CrosstermBackend<BufWriter<io::Stderr>>,
}

impl StderrBackend {
    pub fn new(stderr: io::Stderr) -> Self {
        // Wrap stderr in a `BufWriter` so each backend write coalesces
        // into one syscall on `flush`, matching stdout's buffering.
        Self {
            inner: CrosstermBackend::new(BufWriter::new(stderr)),
        }
    }
}

impl Backend for StderrBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn scroll_region_up(&mut self, region: Range<u16>, amount: u16) -> io::Result<()> {
        self.inner.scroll_region_up(region, amount)
    }

    fn scroll_region_down(&mut self, region: Range<u16>, amount: u16) -> io::Result<()> {
        self.inner.scroll_region_down(region, amount)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    /// Query the cursor position, writing the `ESC[6n` to stderr (not stdout) so a
    /// piped stdout is never corrupted. The reply is read through crossterm's
    /// shared reader; see the module docs for why this is only safe with no live
    /// `EventStream`.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        // Flush buffered cells first: `position_to` writes the `ESC[6n` straight
        // to `io::stderr()`, bypassing `inner`'s `BufWriter`, so anything still
        // buffered would otherwise land after the query.
        Backend::flush(&mut self.inner)?;
        // crossterm already converts the 1-based DSR reply to 0-based (col, row).
        let (col, row) = crossterm::cursor::position_to(io::stderr())?;
        Ok(Position { x: col, y: row })
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}
