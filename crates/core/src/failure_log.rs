//! A failure slot's files as a reader sees them, and the tail of its log.
//!
//! The engine fills a slot (`<home>/failures/<key>/`, see the engine's
//! `failure_slot`); every reader, the engine's own and the renderers in
//! `crates/tui`, reads it through here, so the format has one reader.
//!
//! - [`MANIFEST`] is written last: a slot is complete exactly when it exists.
//!   Its optional `fill_id` names the fill (a uuid minted per fill, P12).
//! - [`LOG`] is the run's log, possibly empty.
//! - [`DIAGNOSTIC`] is the failure's compact record ([`FailureRecord`]),
//!   without its log reference and what it left behind, under a `version` of
//!   its own ([`DIAGNOSTIC_VERSION`]) and the `fill_id` of the slot it was
//!   written for. An unknown version, or another fill's id, reads as no
//!   diagnostic.
//!
//! **A record names its log; it never holds it** (D7b). [`open_tail`] opens
//! the log a [`LogRef`] names and then tells whether it is still that
//! record's, by the fill id (P12, P28): it opens [`LOG`] **first** and reads
//! the manifest after. A slot is built in a temporary directory and renamed
//! into place, so the file opened first can only be as old as the manifest
//! read after it: a match means the open file is this record's log, whatever
//! replaces the slot later. [`stream_tail`] then writes its tail straight to
//! a writer, never through a `String`.
//!
//! **Diagnostics only**: nothing here reaches a hash, and the fill id never
//! enters a cache manifest.

use crate::failure::{FailureRecord, LogRef};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// A slot's manifest, written last.
pub const MANIFEST: &str = "manifest.json";
/// A slot's copy of the run's log.
pub const LOG: &str = "log.txt";
/// A slot's compact failure record.
pub const DIAGNOSTIC: &str = "diagnostic.json";
/// The version of [`DIAGNOSTIC`] this heph writes and reads.
pub const DIAGNOSTIC_VERSION: u32 = 1;

/// The most lines a tail shows, whatever was asked.
pub const TAIL_LINES_CAP: usize = 1000;

/// The most bytes of a log's end a tail holds, whatever its line count (D6.1b):
/// a thousand lines of a minified bundle would otherwise be most of the file,
/// in memory and on every event that carries the tail.
pub const TAIL_MAX_BYTES: usize = 256 * 1024;

/// What a tail starts with when [`TAIL_MAX_BYTES`] cut it inside a line.
pub const TAIL_CUT_MARK: &str = "…";

/// How much of a log [`read_tail_at`] reads per backward step.
const TAIL_CHUNK: u64 = 8 * 1024;

/// How much of the part before the tail [`read_tail_at`] reads per step to
/// count its lines.
const COUNT_CHUNK: usize = 64 * 1024;

/// A bounded slice of a process log prepared for display: the last lines of the
/// full log, plus the real 1-based line number its first line had in the full log
/// so the renderer can show true file positions (e.g. 91–100 for the last 10 of a
/// 100-line log) instead of renumbering from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTail {
    pub text: String,
    /// 1-based line number of `text`'s first line within the full log.
    pub start_line: usize,
}

/// Returns the last `n` lines of `s`, joined with `\n`. A trailing newline does
/// not count as an extra empty line (`str::lines` already drops it).
pub fn last_n_lines(s: &str, n: usize) -> String {
    last_n_lines_with_start(s, n).0
}

/// Like [`last_n_lines`] but also returns the 1-based line number of the first
/// returned line within `s`, so callers can render real file positions rather
/// than renumbering the tail from 1.
pub fn last_n_lines_with_start(s: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    let text = lines.get(start..).unwrap_or(&[]).join("\n");
    (text, start + 1)
}

/// The last `n` lines of a log of `len` bytes, read through `read_at` (a
/// positional read: `read_at(buf, offset)`), with the same text and start
/// line [`last_n_lines_with_start`] gives for the whole log, lossily decoded.
///
/// At most [`TAIL_MAX_BYTES`] of the log are kept, whichever limit comes
/// first. A cut inside a line resumes at a char boundary and starts with
/// [`TAIL_CUT_MARK`]; `start_line` is then the number of the line it cut.
///
/// Seek-based: it reads backwards from the end in [`TAIL_CHUNK`] steps until
/// it holds `n` line breaks, so only the tail is ever held in memory. The
/// start line still needs the number of lines before the tail, which is
/// counted in [`COUNT_CHUNK`] reads that are never kept — constant memory,
/// one pass over the head of the file.
pub fn read_tail_at(
    len: u64,
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    n: usize,
) -> io::Result<LogTail> {
    let (cut, window) = tail_window(len, &mut read_at, n)?;
    let prefix_lines = count_lines_before(&mut read_at, cut)?;
    if n == 0 {
        // No lines shown; the "next" line is one past the last, which an
        // unterminated last line counts as.
        let mut last = [0u8; 1];
        let open = len > 0 && {
            read_exact_at(&mut read_at, &mut last, len - 1)?;
            last != *b"\n"
        };
        return Ok(LogTail {
            text: String::new(),
            start_line: prefix_lines + usize::from(open) + 1,
        });
    }
    let text = String::from_utf8_lossy(&window);
    let (text, local_start) = last_n_lines_with_start(&text, n);
    Ok(LogTail {
        text,
        start_line: prefix_lines + local_start,
    })
}

/// The line breaks in the first `cut` bytes, counted in [`COUNT_CHUNK`]
/// reads that are never kept.
fn count_lines_before(
    read_at: &mut impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    cut: u64,
) -> io::Result<usize> {
    let mut lines = 0usize;
    let mut buf = vec![0u8; COUNT_CHUNK];
    let mut off = 0u64;
    while off < cut {
        let want = usize::try_from((cut - off).min(COUNT_CHUNK as u64)).unwrap_or(COUNT_CHUNK);
        let got = buf.get_mut(..want).unwrap_or_default();
        read_exact_at(read_at, got, off)?;
        lines += got.iter().filter(|&&b| b == b'\n').count();
        off += want as u64;
    }
    Ok(lines)
}

/// The backward half of [`read_tail_at`]: the offset the tail starts at (just
/// past a line break, or 0) and the bytes from there to `len`, at most
/// [`TAIL_MAX_BYTES`] of them. A cut inside a line is the exception: the
/// offset is where the kept part of that line starts, and the bytes start
/// with [`TAIL_CUT_MARK`].
pub fn tail_window(
    len: u64,
    read_at: &mut impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    n: usize,
) -> io::Result<(u64, Vec<u8>)> {
    tail_window_capped(len, read_at, n, TAIL_MAX_BYTES).map(|(start, window, _)| (start, window))
}

/// [`tail_window`], keeping at most `max_bytes`, and saying whether the cut
/// fell inside a line (the window starts with [`TAIL_CUT_MARK`]).
fn tail_window_capped(
    len: u64,
    read_at: &mut impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    n: usize,
    max_bytes: usize,
) -> io::Result<(u64, Vec<u8>, bool)> {
    if n == 0 {
        return Ok((len, Vec::new(), false));
    }
    let mut window: Vec<u8> = Vec::new();
    let mut start = len;
    loop {
        // A line break that ends the log closes its last line rather than
        // starting another, as `str::lines` has it.
        let trailing = usize::from(window.last() == Some(&b'\n'));
        let body = window.get(..window.len() - trailing).unwrap_or_default();
        // The n-th break from the end precedes the first line of the tail.
        if let Some((i, _)) = body
            .iter()
            .enumerate()
            .rev()
            .filter(|&(_, &b)| b == b'\n')
            .nth(n - 1)
        {
            let tail = window.split_off(i + 1);
            return Ok(cap_window(start + i as u64 + 1, tail, max_bytes));
        }
        // Past the cap with the byte before the cut in hand, which tells a
        // cut at a line break from one inside a line.
        if start == 0 || window.len() > max_bytes {
            return Ok(cap_window(start, window, max_bytes));
        }
        let take = start.min(TAIL_CHUNK);
        let off = start - take;
        let mut chunk = vec![0u8; usize::try_from(take).unwrap_or(0)];
        read_exact_at(read_at, &mut chunk, off)?;
        chunk.extend_from_slice(&window);
        window = chunk;
        start = off;
    }
}

/// `window`, which starts at `start` in the log, cut to its last
/// `max_bytes`. A cut right after a line break keeps whole lines; one inside
/// a line moves forward to a char boundary and is marked (the `bool`).
fn cap_window(start: u64, mut window: Vec<u8>, max_bytes: usize) -> (u64, Vec<u8>, bool) {
    let Some(drop) = window.len().checked_sub(max_bytes).filter(|&drop| drop > 0) else {
        return (start, window, false);
    };
    let at_break = window.get(drop - 1) == Some(&b'\n');
    let mut cut = drop;
    if !at_break {
        // A UTF-8 char is at most 4 bytes: at most 3 continuation bytes to skip.
        while cut < drop + 3 && window.get(cut).is_some_and(|b| b & 0xC0 == 0x80) {
            cut += 1;
        }
    }
    let kept = window.split_off(cut);
    if at_break {
        return (start + cut as u64, kept, false);
    }
    let mut marked = Vec::with_capacity(TAIL_CUT_MARK.len() + kept.len());
    marked.extend_from_slice(TAIL_CUT_MARK.as_bytes());
    marked.extend_from_slice(&kept);
    (start + cut as u64, marked, true)
}

fn read_exact_at(
    read_at: &mut impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    mut buf: &mut [u8],
    mut off: u64,
) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(buf, off) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "log shrank while its tail was read",
                ));
            }
            Ok(k) => {
                buf = buf.get_mut(k..).unwrap_or_default();
                off += k as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// How many lines `file` holds, as `str::lines` counts them: every line
/// break, plus an unterminated last line. Positional reads in
/// [`COUNT_CHUNK`] steps, so the cursor is untouched and memory constant.
pub fn count_lines(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::FileExt as _;
    let len = file.metadata()?.len();
    let mut read_at = |buf: &mut [u8], off: u64| file.read_at(buf, off);
    let breaks = count_lines_before(&mut read_at, len)? as u64;
    let open = len > 0 && {
        let mut last = [0u8; 1];
        read_exact_at(&mut read_at, &mut last, len - 1)?;
        last != *b"\n"
    };
    Ok(breaks + u64::from(open))
}

/// What [`stream_tail`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailStats {
    /// The 1-based number, in the whole log, of the first line written.
    pub start_line: u64,
    /// How many lines were written.
    pub lines: usize,
    /// The byte cap cut the first line written: it starts with
    /// [`TAIL_CUT_MARK`].
    pub cut: bool,
}

/// Write the last `n` lines of `file` to `out`: at most [`TAIL_LINES_CAP`]
/// lines and [`TAIL_MAX_BYTES`] bytes, whatever `n` is, cut as
/// [`read_tail_at`] cuts them.
///
/// Seeks to the end and scans back, holding at most the capped window; the
/// tail is never collected into a `String`. The bytes are the log's own (no
/// decoding, no escaping: the renderer escapes), the lines joined by the
/// log's `\n` with its final one left off. Reads through positional reads,
/// so `file`'s cursor is untouched and a shared descriptor works.
pub fn stream_tail(file: &File, n: usize, out: &mut dyn Write) -> io::Result<TailStats> {
    stream_tail_capped(file, n, TAIL_MAX_BYTES, out)
}

/// [`stream_tail`] with a byte cap of its own, itself within
/// [`TAIL_MAX_BYTES`].
pub fn stream_tail_capped(
    file: &File,
    n: usize,
    max_bytes: usize,
    out: &mut dyn Write,
) -> io::Result<TailStats> {
    use std::os::unix::fs::FileExt as _;
    let n = n.min(TAIL_LINES_CAP);
    let max_bytes = max_bytes.min(TAIL_MAX_BYTES);
    let len = file.metadata()?.len();
    let mut read_at = |buf: &mut [u8], off: u64| file.read_at(buf, off);
    let (start, window, cut) = tail_window_capped(len, &mut read_at, n, max_bytes)?;
    let start_line = count_lines_before(&mut read_at, start)? as u64 + 1;
    let body = window.strip_suffix(b"\n").unwrap_or(&window);
    out.write_all(body)?;
    let lines = if window.is_empty() {
        0
    } else {
        body.iter().filter(|&&b| b == b'\n').count() + 1
    };
    Ok(TailStats {
        start_line,
        lines,
        cut,
    })
}

/// The `fill_id` in the manifest of the slot at `dir`: `Ok(None)` for a
/// manifest with none (a slot from an older heph). An error when there is no
/// manifest (`NotFound`: no slot, or one being replaced) or it does not
/// decode (`InvalidData`).
pub fn peek_fill_id(dir: &Path) -> io::Result<Option<String>> {
    #[derive(Deserialize)]
    struct Peek {
        #[serde(default)]
        fill_id: Option<String>,
    }
    let bytes = std::fs::read(dir.join(MANIFEST))?;
    let peek: Peek = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(peek.fill_id)
}

/// A log [`open_tail`] opened, to stream with [`stream_tail`].
#[derive(Debug)]
pub struct OpenTail {
    /// The log, opened before its slot's manifest was read: this fill's
    /// bytes whatever replaces the slot afterwards.
    pub file: File,
    /// The lines asked for, within [`TAIL_LINES_CAP`].
    pub lines: usize,
    /// The bytes asked for, within [`TAIL_MAX_BYTES`].
    pub max_bytes: usize,
}

impl OpenTail {
    /// Write the tail asked for to `out` ([`stream_tail_capped`]).
    pub fn stream(&self, out: &mut dyn Write) -> io::Result<TailStats> {
        stream_tail_capped(&self.file, self.lines, self.max_bytes, out)
    }
}

/// What reading a record's log found.
#[derive(Debug)]
pub enum TailRead {
    /// The record's own log, or for a reader with no expected fill id, the
    /// slot's current one.
    Stream(OpenTail),
    /// The slot was filled again since: by a newer failed run, or by an
    /// older heph that writes no fill id.
    Replaced,
    /// The slot is gone: a later run passed, or it was cleaned.
    Cleared,
    /// A reader with no expected fill id met a slot with none (an older
    /// heph's): the log is shown, with no claim about whose it is.
    CannotTell(OpenTail),
    /// The log or its manifest exists and could not be read. Not a record
    /// with no log on disk at all: see [`open_tail`].
    Unreadable(io::Error),
}

/// Open the log `log` names, to stream its last `lines` lines (at most
/// `max_bytes`), and tell whether it is still that record's (P12, P28).
///
/// `log.txt` is opened first, then the manifest's fill id is read and
/// compared with `log.fill_id`:
/// - the same id: [`TailRead::Stream`];
/// - another id, or none where the record has one: [`TailRead::Replaced`];
/// - the record has none: [`TailRead::Stream`] when the manifest has one (the
///   reader asked for whatever the slot holds), [`TailRead::CannotTell`]
///   when it has none either;
/// - no `log.txt` or no manifest: [`TailRead::Cleared`].
///
/// A record with no log on disk (`log.path` is `None`) reads as
/// [`TailRead::Cleared`]; a caller with an in-memory tail
/// ([`LogRef::memory_tail`]) shows that instead.
pub fn open_tail(log: &LogRef, lines: usize, max_bytes: usize) -> TailRead {
    let Some(path) = log.path.as_deref().map(Path::new) else {
        return TailRead::Cleared;
    };
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return TailRead::Cleared,
        Err(e) => return TailRead::Unreadable(e),
    };
    let Some(dir) = path.parent() else {
        return TailRead::Cleared;
    };
    let found = match peek_fill_id(dir) {
        Ok(id) => id,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return TailRead::Cleared,
        Err(e) => return TailRead::Unreadable(e),
    };
    let open = OpenTail {
        file,
        lines: lines.min(TAIL_LINES_CAP),
        max_bytes: max_bytes.min(TAIL_MAX_BYTES),
    };
    match (log.fill_id.as_deref(), found.as_deref()) {
        (Some(want), Some(got)) if want == got => TailRead::Stream(open),
        (Some(_), _) => TailRead::Replaced,
        (None, Some(_)) => TailRead::Stream(open),
        (None, None) => TailRead::CannotTell(open),
    }
}

/// The tail of the log `log` names as text, for a consumer that has to hold
/// it (a PR comment's code block), within `lines` and `max_bytes`: the
/// record's in-memory tail when it has one (credential material, E2), else
/// what [`open_tail`] streams of this record's log. `None` when the log was
/// replaced, cleared or cannot be read: that consumer shows no tail rather
/// than another run's.
///
/// Read once per call and dropped by the caller: nothing keeps it.
pub fn read_tail(log: &LogRef, lines: usize, max_bytes: usize) -> Option<LogTail> {
    if let Some(tail) = &log.memory_tail {
        let (text, start) = last_n_lines_with_start(tail, lines.min(TAIL_LINES_CAP));
        return Some(LogTail {
            text,
            start_line: start,
        });
    }
    let open = match open_tail(log, lines, max_bytes) {
        TailRead::Stream(open) | TailRead::CannotTell(open) => open,
        TailRead::Replaced | TailRead::Cleared | TailRead::Unreadable(_) => return None,
    };
    let mut bytes = Vec::new();
    let stats = open.stream(&mut bytes).ok()?;
    Some(LogTail {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        start_line: usize::try_from(stats.start_line).unwrap_or(usize::MAX),
    })
}

/// [`DIAGNOSTIC`]'s envelope, as written.
#[derive(Serialize)]
struct DiagnosticOut<'a> {
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    fill_id: Option<&'a str>,
    #[serde(flatten)]
    record: &'a FailureRecord,
}

/// [`DIAGNOSTIC`]'s envelope, as read before the record is.
#[derive(Deserialize)]
struct DiagnosticHead {
    version: u32,
    #[serde(default)]
    fill_id: Option<String>,
}

/// `record` as [`DIAGNOSTIC`]'s bytes, for the slot filled as `fill_id`:
/// without its log reference and what it left behind, which are the slot's
/// own files.
pub fn encode_diagnostic(
    record: &FailureRecord,
    fill_id: Option<&str>,
) -> serde_json::Result<Vec<u8>> {
    let mut record = record.clone();
    record.log = None;
    record.left_behind = None;
    serde_json::to_vec(&DiagnosticOut {
        version: DIAGNOSTIC_VERSION,
        fill_id,
        record: &record,
    })
}

/// The record in the slot at `dir`'s [`DIAGNOSTIC`], when there is one this
/// heph reads, written for the slot's current fill: an unknown version, a
/// fill id other than the manifest's, or a file that does not decode reads
/// as none.
pub fn read_diagnostic(dir: &Path) -> Option<FailureRecord> {
    let bytes = std::fs::read(dir.join(DIAGNOSTIC)).ok()?;
    let head: DiagnosticHead = serde_json::from_slice(&bytes).ok()?;
    if head.version != DIAGNOSTIC_VERSION || head.fill_id != peek_fill_id(dir).ok()? {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{Diagnostic, Stage};

    fn tail_of_bytes(bytes: &[u8], n: usize) -> LogTail {
        read_tail_at(
            bytes.len() as u64,
            |buf, off| {
                let src = bytes
                    .get(usize::try_from(off).unwrap_or(usize::MAX)..)
                    .unwrap_or_default();
                let k = src.len().min(buf.len());
                buf.get_mut(..k)
                    .unwrap_or_default()
                    .copy_from_slice(src.get(..k).unwrap_or_default());
                Ok(k)
            },
            n,
        )
        .expect("tail")
    }

    /// `bytes` in a file of its own, and what [`stream_tail`] writes of its
    /// last `n` lines.
    fn streamed(bytes: &[u8], n: usize) -> (Vec<u8>, TailStats) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LOG);
        std::fs::write(&path, bytes).expect("write log");
        let file = File::open(&path).expect("open log");
        let mut out = Vec::new();
        let stats = stream_tail(&file, n, &mut out).expect("stream");
        (out, stats)
    }

    /// `count_lines` agrees with `str::lines` on every shape of log, one
    /// that spans several count chunks included.
    #[test]
    fn count_lines_matches_str_lines() {
        let long: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        for log in ["", "\n", "a", "a\n", "a\nb", "a\n\nb\n", long.as_str()] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join(LOG);
            std::fs::write(&path, log).expect("write log");
            let file = File::open(&path).expect("open log");
            assert_eq!(
                count_lines(&file).expect("count"),
                log.lines().count() as u64,
                "{:?}",
                log.get(..20)
            );
        }
    }

    /// 1000 lines of 1 KiB are ~1 MiB: the tail keeps the last 256 KiB. A cut
    /// inside a line resumes at a char boundary (a 3-byte `€` is never split)
    /// and is marked, and `start_line` is the number of the line it cut. A cut
    /// that lands right after a break keeps whole lines and has no mark.
    #[test]
    fn tail_byte_cap_256k_with_long_lines() {
        let line = "€".repeat(340); // 1020 bytes, +1 for the break.
        let log: String = (0..1000).map(|_| format!("{line}\n")).collect();
        let tail = tail_of_bytes(log.as_bytes(), 1000);
        assert!(tail.text.starts_with(TAIL_CUT_MARK), "the cut is marked");
        // The cap falls 253 bytes into line 744, mid-`€`: two continuation
        // bytes are skipped (and the text drops the log's final break).
        assert_eq!(
            tail.text.len() - TAIL_CUT_MARK.len(),
            TAIL_MAX_BYTES - 2 - 1
        );
        assert_eq!(tail.start_line, 744);
        assert!(!tail.text.contains('\u{fffd}'), "no char was split");
        assert!(tail.text.ends_with(&line), "the last line is whole");

        // Lines of exactly 1 KiB: 256 of them are the cap, cut at a break.
        let line = "x".repeat(1023);
        let log: String = (0..1000).map(|_| format!("{line}\n")).collect();
        let tail = tail_of_bytes(log.as_bytes(), 1000);
        assert!(
            !tail.text.starts_with(TAIL_CUT_MARK),
            "whole lines: no mark"
        );
        assert_eq!(tail.text.lines().count(), 256);
        assert_eq!(tail.start_line, 1000 - 256 + 1);

        // One line past the cap and no break at all.
        let log = "y".repeat(TAIL_MAX_BYTES + 10);
        let tail = tail_of_bytes(log.as_bytes(), 1000);
        assert_eq!(tail.text.len(), TAIL_CUT_MARK.len() + TAIL_MAX_BYTES);
        assert_eq!(tail.start_line, 1);

        // Exactly the cap is kept whole; one byte over is cut and marked.
        let log = "y".repeat(TAIL_MAX_BYTES);
        let tail = tail_of_bytes(log.as_bytes(), 1000);
        assert_eq!((tail.text.as_str(), tail.start_line), (log.as_str(), 1));
        let log = "y".repeat(TAIL_MAX_BYTES + 1);
        let tail = tail_of_bytes(log.as_bytes(), 1000);
        assert_eq!(tail.text.len(), TAIL_CUT_MARK.len() + TAIL_MAX_BYTES);
        assert!(tail.text.starts_with(TAIL_CUT_MARK));

        // The n-th break found, and the tail after it still over the cap:
        // cut inside its first line, which is line 2.
        let log = format!("a\n{}\nc\n", "b".repeat(TAIL_MAX_BYTES + 100));
        let tail = tail_of_bytes(log.as_bytes(), 2);
        assert!(tail.text.starts_with(TAIL_CUT_MARK), "the cut is marked");
        assert!(tail.text.ends_with("b\nc"), "the last line is whole");
        assert_eq!(tail.text.len(), TAIL_CUT_MARK.len() + TAIL_MAX_BYTES - 1);
        assert_eq!(tail.start_line, 2);
    }

    /// However many lines are asked for, a streamed tail is at most 1000 of
    /// them, numbered as the whole log numbers them.
    #[test]
    fn tail_line_cap_1000() {
        let log: String = (1..=2500).map(|i| format!("line {i}\n")).collect();
        for asked in [1000, 1001, 5000, usize::MAX] {
            let (out, stats) = streamed(log.as_bytes(), asked);
            let text = String::from_utf8(out).expect("utf-8");
            assert_eq!(text.lines().count(), TAIL_LINES_CAP, "asked {asked}");
            assert_eq!(
                stats,
                TailStats {
                    start_line: 1501,
                    lines: 1000,
                    cut: false,
                },
                "asked {asked}"
            );
            assert!(text.starts_with("line 1501\n") && text.ends_with("line 2500"));
        }
        // Fewer asked: those.
        let (out, stats) = streamed(log.as_bytes(), 3);
        assert_eq!(out, b"line 2498\nline 2499\nline 2500");
        assert_eq!((stats.start_line, stats.lines), (2498, 3));
    }

    /// A streamed tail is at most 256 KiB: a cut inside a line resumes at a
    /// char boundary and is marked; one at a break keeps whole lines. The
    /// same cut [`read_tail_at`] makes, and a cap of the caller's own never
    /// goes past it.
    #[test]
    fn tail_byte_cap_256k() {
        let line = "€".repeat(340);
        let log: String = (0..1000).map(|_| format!("{line}\n")).collect();
        let (out, stats) = streamed(log.as_bytes(), 1000);
        let oracle = tail_of_bytes(log.as_bytes(), 1000);
        assert_eq!(String::from_utf8(out).expect("no char split"), oracle.text);
        assert_eq!(stats.start_line, 744);
        assert!(stats.cut);

        let line = "x".repeat(1023);
        let log: String = (0..1000).map(|_| format!("{line}\n")).collect();
        let (out, stats) = streamed(log.as_bytes(), 1000);
        assert!(out.len() < TAIL_MAX_BYTES);
        assert!(!stats.cut, "cut at a break: whole lines");
        assert_eq!((stats.start_line, stats.lines), (745, 256));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LOG);
        std::fs::write(&path, "z".repeat(4 * TAIL_MAX_BYTES)).expect("write");
        let file = File::open(&path).expect("open");
        let mut out = Vec::new();
        let stats = stream_tail_capped(&file, 10, usize::MAX, &mut out).expect("stream");
        assert_eq!(
            out.len(),
            TAIL_CUT_MARK.len() + TAIL_MAX_BYTES,
            "caller's cap is capped"
        );
        assert!(stats.cut);
        let mut out = Vec::new();
        stream_tail_capped(&file, 10, 16, &mut out).expect("stream");
        assert_eq!(out, format!("{TAIL_CUT_MARK}{}", "z".repeat(16)).as_bytes());
    }

    /// I8: [`read_tail_at`] holds the tail, never the log. Over a 4 MiB log,
    /// with the line cap binding (64-byte lines) and with the byte cap
    /// binding (1 KiB lines): the backward pass reads at most
    /// [`TAIL_MAX_BYTES`] + one [`TAIL_CHUNK`] from the end, the count pass
    /// reads the head before the tail once, and no read asks for more than
    /// [`COUNT_CHUNK`] at a time — so what is in memory at once is bounded
    /// by the caps, whatever the log's size.
    #[test]
    fn read_tail_at_holds_the_tail_not_the_log() {
        const LOG_BYTES: usize = 4 * 1024 * 1024;
        for line_len in [64, 1024] {
            let line = format!("{}\n", "x".repeat(line_len - 1));
            let log = line.repeat(LOG_BYTES / line_len);
            let len = log.len() as u64;
            // (offset, length) of every read, in order.
            let mut reads: Vec<(u64, usize)> = Vec::new();
            let tail = read_tail_at(
                len,
                |buf, off| {
                    let start = usize::try_from(off).expect("offset");
                    let k = buf.len().min(log.len() - start);
                    buf[..k].copy_from_slice(&log.as_bytes()[start..start + k]);
                    reads.push((off, k));
                    Ok(k)
                },
                TAIL_LINES_CAP,
            )
            .expect("tail");
            // The backward pass stops far short of the start of a 4 MiB log;
            // the count pass starts at 0 and climbs.
            let back = reads.iter().position(|r| r.0 == 0).expect("a count pass");
            let (backward, count) = reads.split_at(back);
            let backward_bytes: usize = backward.iter().map(|r| r.1).sum();
            let count_bytes: usize = count.iter().map(|r| r.1).sum();
            assert!(
                backward_bytes <= TAIL_MAX_BYTES + TAIL_CHUNK as usize,
                "{line_len}-byte lines: read {backward_bytes} B back from the end"
            );
            assert!(
                reads.iter().all(|r| r.1 <= COUNT_CHUNK),
                "{line_len}-byte lines: a read over {COUNT_CHUNK} B"
            );
            assert!(tail.text.len() <= TAIL_MAX_BYTES, "{line_len}-byte lines");
            let lines_before = tail.start_line - 1;
            assert_eq!(
                count_bytes,
                lines_before * line_len,
                "{line_len}-byte lines: the head is read once, to count its lines"
            );
            assert!(
                log.trim_end_matches('\n').ends_with(&tail.text),
                "{line_len}-byte lines: the tail is the log's end"
            );
        }
    }

    /// The streamed tail is the text [`read_tail_at`] gives, for logs at the
    /// edges: empty, no final break, one line, blank lines.
    #[test]
    fn stream_tail_matches_read_tail_at() {
        for (log, n) in [
            ("", 5),
            ("a", 5),
            ("a\n", 5),
            ("a\nb", 1),
            ("a\n\n\nb\n", 2),
            ("a\nb\nc\n", 0),
        ] {
            let (out, stats) = streamed(log.as_bytes(), n);
            let oracle = tail_of_bytes(log.as_bytes(), n);
            assert_eq!(
                String::from_utf8(out).expect("utf-8"),
                oracle.text,
                "{log:?} {n}"
            );
            if n > 0 && !log.is_empty() {
                assert_eq!(stats.start_line as usize, oracle.start_line, "{log:?} {n}");
            }
        }
        let (_, stats) = streamed(b"", 5);
        assert_eq!((stats.start_line, stats.lines), (1, 0));
    }

    fn slot(dir: &Path, fill_id: Option<&str>, log: &str) -> LogRef {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join(LOG), log).expect("log");
        let manifest = match fill_id {
            Some(id) => format!(r#"{{"version":1,"addr":"//p:t","failed_at":1,"fill_id":"{id}"}}"#),
            None => r#"{"version":1,"addr":"//p:t","failed_at":1}"#.to_owned(),
        };
        std::fs::write(dir.join(MANIFEST), manifest).expect("manifest");
        LogRef {
            path: Some(dir.join(LOG).display().to_string()),
            fill_id: fill_id.map(str::to_owned),
            lines: 1,
            memory_tail: None,
        }
    }

    /// The fill-id reading rule (P28), every row of it.
    #[test]
    fn open_tail_reads_the_fill_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("k");
        let mine = slot(&dir, Some("a"), "mine\n");
        let TailRead::Stream(open) = open_tail(&mine, 10, TAIL_MAX_BYTES) else {
            panic!("the record's own fill streams");
        };
        let mut out = Vec::new();
        open.stream(&mut out).expect("stream");
        assert_eq!(out, b"mine");

        slot(&dir, Some("b"), "newer\n");
        assert!(matches!(open_tail(&mine, 10, 100), TailRead::Replaced));
        let reader = LogRef {
            fill_id: None,
            ..mine.clone()
        };
        assert!(matches!(open_tail(&reader, 10, 100), TailRead::Stream(_)));

        slot(&dir, None, "older heph\n");
        assert!(matches!(open_tail(&mine, 10, 100), TailRead::Replaced));
        assert!(matches!(
            open_tail(&reader, 10, 100),
            TailRead::CannotTell(_)
        ));

        std::fs::remove_file(dir.join(MANIFEST)).expect("rm manifest");
        assert!(matches!(open_tail(&mine, 10, 100), TailRead::Cleared));
        std::fs::remove_dir_all(&dir).expect("rm slot");
        assert!(matches!(open_tail(&mine, 10, 100), TailRead::Cleared));
        assert!(matches!(
            open_tail(&LogRef::default(), 10, 100),
            TailRead::Cleared
        ));
    }

    /// `read_tail` gives the record's own log as text, its in-memory tail
    /// first, and nothing for another fill's log or a cleared slot.
    #[test]
    fn read_tail_reads_only_the_records_own_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("k");
        let mine = slot(&dir, Some("a"), "one\ntwo\nthree\n");
        let tail = read_tail(&mine, 2, TAIL_MAX_BYTES).expect("own log");
        assert_eq!((tail.text.as_str(), tail.start_line), ("two\nthree", 2));

        let memory = LogRef {
            memory_tail: Some("x\ny".into()),
            ..mine.clone()
        };
        assert_eq!(read_tail(&memory, 1, 100).expect("memory").text, "y");

        slot(&dir, Some("b"), "newer\n");
        assert_eq!(read_tail(&mine, 2, 100), None, "replaced");
        std::fs::remove_dir_all(&dir).expect("rm slot");
        assert_eq!(read_tail(&mine, 2, 100), None, "cleared");
    }

    /// `diagnostic.json` round-trips without the slot's own files, and reads
    /// as none under an unknown version or for another fill.
    #[test]
    fn diagnostic_reads_only_its_own_fill_and_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let mut rec = FailureRecord::new(Diagnostic::new(Stage::Parse, "bad"));
        rec.log = Some(LogRef::default());
        rec.left_behind = Some(crate::events::LeftBehindData::default());
        slot(dir, Some("a"), "");
        std::fs::write(
            dir.join(DIAGNOSTIC),
            encode_diagnostic(&rec, Some("a")).expect("encode"),
        )
        .expect("write");
        let back = read_diagnostic(dir).expect("reads");
        assert_eq!(back.log, None);
        assert_eq!(back.left_behind, None);
        assert_eq!(back.diagnostic, rec.diagnostic);

        slot(dir, Some("b"), "");
        assert_eq!(read_diagnostic(dir), None, "another fill's");

        std::fs::write(
            dir.join(DIAGNOSTIC),
            r#"{"version":2,"fill_id":"b","diagnostic":{"stage":"run","message":"m"}}"#,
        )
        .expect("write");
        assert_eq!(read_diagnostic(dir), None, "an unknown version");
    }
}
