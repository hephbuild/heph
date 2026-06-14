//! ratatui [`Backend`] that wraps [`CrosstermBackend<io::Stderr>`] and
//! intercepts `get_cursor_position` so the DSR query never touches stdout.
//!
//! crossterm 0.29's `cursor::position()` hardcodes `io::stdout()` for its
//! `\x1b[6n` write (see `crossterm/src/cursor/sys/unix.rs`). ratatui's
//! `Inline` viewport calls it from `Terminal::with_options`, `resize`,
//! and (via `autoresize`) every `draw`. When stdout is piped
//! (`cmd | wc -l`) the escape lands in the pipe, corrupting the
//! consumer's input and starving crossterm's response wait.
//!
//! This backend keeps stdout reserved for the app's real output: the DSR
//! query is written directly to stderr (the tty the TUI is already
//! drawing on), and the response is read straight from `/dev/tty`.
//! Every other backend operation forwards unchanged to the inner
//! `CrosstermBackend`.
//!
//! Safety: the manual `/dev/tty` reader must not race with crossterm's
//! global event reader. Callers (the interactive TUI backend) ensure
//! `EventStream` is not alive across `get_cursor_position` calls — it is
//! created after the initial `Terminal::with_options` and dropped before
//! the resume-time `Terminal::with_options`.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct StderrBackend {
    inner: CrosstermBackend<BufWriter<io::Stderr>>,
    /// The inline viewport's cursor anchor, captured on the first
    /// `get_cursor_position` and reused for this backend's lifetime. See that
    /// method for why a *live* DSR query is only safe at backend construction.
    cached_cursor: Option<Position>,
}

impl StderrBackend {
    pub fn new(stderr: io::Stderr) -> Self {
        // Wrap stderr in a `BufWriter` so each backend write coalesces
        // into one syscall on `flush`, matching stdout's buffering.
        Self {
            inner: CrosstermBackend::new(BufWriter::new(stderr)),
            cached_cursor: None,
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

    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, amount: u16) -> io::Result<()> {
        self.inner.scroll_region_up(region, amount)
    }

    fn scroll_region_down(&mut self, region: std::ops::Range<u16>, amount: u16) -> io::Result<()> {
        self.inner.scroll_region_down(region, amount)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    /// Cursor position for the inline viewport anchor.
    ///
    /// A *live* DSR query (`\x1b[6n` → read the reply off `/dev/tty`) races
    /// crossterm's `EventStream` reader, which consumes the same tty input:
    /// whoever reads first wins, and a lost reply blocks/stalls the query.
    /// heph guarantees no `EventStream` is alive only at backend construction
    /// (initial `with_options`, and the resize/resume rebuilds, which tear the
    /// stream down first). Newer ratatui, however, issues this query from many
    /// in-loop paths (`autoresize` on any size delta, `clear`, `insert_before`)
    /// while the stream *is* alive — the freeze this guards against.
    ///
    /// The anchor is fixed for an inline viewport's lifetime (a real resize
    /// builds a fresh backend), so we query the terminal exactly once — on the
    /// first call, at construction time, which is the race-free window — and
    /// serve every later call from the cache. No in-loop call ever races.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        if let Some(pos) = self.cached_cursor {
            return Ok(pos);
        }
        // Drain any buffered backend writes first — the DSR query below
        // writes raw to `io::stderr()`, bypassing `inner`'s `BufWriter`,
        // so unflushed cells would otherwise land after the query.
        Backend::flush(&mut self.inner)?;
        let pos = query_cursor_position_via_stderr()?;
        self.cached_cursor = Some(pos);
        Ok(pos)
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

const DSR_TIMEOUT: Duration = Duration::from_millis(2000);

fn query_cursor_position_via_stderr() -> io::Result<Position> {
    // Send the DSR query down stderr — the same tty the TUI already
    // renders to — instead of crossterm's hardcoded `io::stdout()`.
    {
        let mut stderr = io::stderr().lock();
        stderr.write_all(b"\x1b[6n")?;
        stderr.flush()?;
    }
    let response = read_dsr_response(DSR_TIMEOUT)?;
    let (col_1_based, row_1_based) = parse_dsr(&response).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed DSR response: {response:?}"),
        )
    })?;
    Ok(Position {
        x: col_1_based.saturating_sub(1),
        y: row_1_based.saturating_sub(1),
    })
}

/// Read bytes from `/dev/tty` until a complete `\x1b[<row>;<col>R`
/// sequence has been seen, or `timeout` elapses. Any bytes that arrive
/// before the response is discarded — the caller (which only invokes
/// this while crossterm's `EventStream` is not alive) guarantees there
/// are no other consumers of `/dev/tty`.
fn read_dsr_response(timeout: Duration) -> io::Result<Vec<u8>> {
    let tty = File::open("/dev/tty")?;
    let fd = tty.as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(32);
    let mut scratch = [0u8; 32];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "DSR response timed out",
            ));
        }
        let timeout_ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd points to a valid pollfd on the stack; nfds = 1.
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if r == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "DSR response timed out",
            ));
        }
        // SAFETY: fd is open and owned by `tty`; scratch buffer is valid.
        let n = unsafe { libc::read(fd, scratch.as_mut_ptr().cast(), scratch.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "tty eof"));
        }
        let n_usize = n.cast_unsigned();
        if let Some(slice) = scratch.get(..n_usize) {
            buf.extend_from_slice(slice);
        }
        if has_complete_dsr(&buf) {
            return Ok(buf);
        }
    }
}

/// True if `buf` contains a complete `\x1b[…R` sequence somewhere.
fn has_complete_dsr(buf: &[u8]) -> bool {
    let mut idx = 0;
    while let (Some(&a), Some(&b)) = (buf.get(idx), buf.get(idx + 1)) {
        if a == 0x1b && b == b'[' {
            // Scan after `\x1b[` for the terminating 'R'. If we hit a
            // byte that's neither a digit, ';', nor 'R' we abort this
            // start position — it's not a DSR sequence.
            for &c in buf.iter().skip(idx + 2) {
                if c == b'R' {
                    return true;
                }
                if !(c.is_ascii_digit() || c == b';') {
                    break;
                }
            }
        }
        idx += 1;
    }
    false
}

/// Extract `(col, row)` from the first complete `\x1b[<row>;<col>R`.
/// Returns 1-based values as reported by the terminal.
fn parse_dsr(buf: &[u8]) -> Option<(u16, u16)> {
    let start = buf.windows(2).position(|w| w == b"\x1b[")?;
    let after = buf.get(start + 2..)?;
    let end = after.iter().position(|&b| b == b'R')?;
    let body = std::str::from_utf8(after.get(..end)?).ok()?;
    let mut parts = body.split(';');
    let row: u16 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    Some((col, row))
}

#[cfg(test)]
mod tests {
    use super::{has_complete_dsr, parse_dsr};

    #[test]
    fn parse_dsr_extracts_col_row() {
        assert_eq!(parse_dsr(b"\x1b[12;34R"), Some((34, 12)));
        assert_eq!(parse_dsr(b"prefix\x1b[1;1Rsuffix"), Some((1, 1)));
    }

    #[test]
    fn parse_dsr_returns_none_on_garbage() {
        assert_eq!(parse_dsr(b""), None);
        assert_eq!(parse_dsr(b"\x1b[notnumbers;R"), None);
        assert_eq!(parse_dsr(b"\x1b[12R"), None);
    }

    #[test]
    fn has_complete_dsr_detects_finished_response() {
        assert!(has_complete_dsr(b"\x1b[12;34R"));
        assert!(has_complete_dsr(b"trailing\x1b[1;1R"));
    }

    #[test]
    fn has_complete_dsr_rejects_partial() {
        assert!(!has_complete_dsr(b"\x1b[12;3"));
        assert!(!has_complete_dsr(b"no escape here"));
        // Letter that isn't 'R' breaks out without matching.
        assert!(!has_complete_dsr(b"\x1b[12;34X"));
    }
}
