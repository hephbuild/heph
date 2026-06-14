//! ratatui [`Backend`] that wraps [`CrosstermBackend<io::Stderr>`] and
//! intercepts `get_cursor_position`.
//!
//! Two reasons this exists rather than using the stock crossterm backend:
//!
//! 1. **DSR query on stderr, not stdout.** ratatui's inline viewport anchors
//!    itself with a `ESC[6n` cursor query. crossterm's `cursor::position()`
//!    writes that to `io::stdout()`, which corrupts a piped stdout
//!    (`heph … | cmd`). We use the fork's [`cursor::position_to`] to send it to
//!    stderr (the tty the TUI already draws on) while still reading the reply
//!    through crossterm's shared input reader (so it demuxes from key events).
//!
//! 2. **One query, cached.** `cursor::position_to` reads via crossterm's single
//!    global reader, which a live `EventStream` monopolises — so it can only run
//!    when no `EventStream` is active (at startup before one exists, and during
//!    a resize where the loop drops it). The inline anchor is fixed for the
//!    backend's lifetime anyway, so we query exactly once and cache it. That is
//!    also the *correct* value for ratatui's other caller of this method:
//!    `clear()`'s save/restore at teardown wants the viewport anchor, not the
//!    live (bottom-of-box) cursor.

use std::io::{self, BufWriter};
use std::ops::Range;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct StderrBackend {
    inner: CrosstermBackend<BufWriter<io::Stderr>>,
    /// The inline viewport anchor, captured on the first query and reused for
    /// this backend's lifetime. See the module docs and [`Self::get_cursor_position`].
    cached_anchor: Option<Position>,
}

impl StderrBackend {
    pub fn new(stderr: io::Stderr) -> Self {
        // Wrap stderr in a `BufWriter` so each backend write coalesces
        // into one syscall on `flush`, matching stdout's buffering.
        Self {
            inner: CrosstermBackend::new(BufWriter::new(stderr)),
            cached_anchor: None,
        }
    }

    /// Drop the cached anchor so the next query re-reads it. A resize moves the
    /// anchor; the loop invalidates after tearing down the `EventStream` for the
    /// resize re-anchor (see `reanchor_after_resize`).
    pub fn invalidate_cursor_cache(&mut self) {
        self.cached_anchor = None;
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

    /// Cursor position for the inline viewport anchor — queried once via stderr,
    /// then cached (see module docs for why a single cached value is both
    /// necessary and correct here).
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        if let Some(anchor) = self.cached_anchor {
            return Ok(anchor);
        }
        // Flush buffered cells first: `position_to` writes the `ESC[6n` straight
        // to `io::stderr()`, bypassing `inner`'s `BufWriter`, so anything still
        // buffered would otherwise land after the query.
        Backend::flush(&mut self.inner)?;
        // crossterm already converts the 1-based DSR reply to 0-based (col, row).
        let (col, row) = crossterm::cursor::position_to(io::stderr())?;
        let anchor = Position { x: col, y: row };
        self.cached_anchor = Some(anchor);
        Ok(anchor)
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
