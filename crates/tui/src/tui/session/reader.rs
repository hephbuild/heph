//! Reading `events.ndjson` back.
//!
//! The rules (I4, design amendment A5):
//!
//! - A record is a *complete* line. A trailing line with no newline yet is the
//!   owner mid-write: it stays buffered, and the next call after the file grows
//!   picks it up. Nothing is lost or decoded early.
//! - A complete line that does not decode is skipped and **counted**, and
//!   reading goes on. This is the opposite of the plugin SDK's pull loop, which
//!   stops at the first bad frame: a file a newer heph appended to must not end
//!   at the first record this heph does not understand.
//! - A line with neither `kind` nor `session` is skipped (not counted: it
//!   decoded, it just carries nothing for this heph).
//! - No line-length cap. A long log line is still one record.

use std::io::{self, Read};

use super::format::SessionRecord;

/// Bytes read from the source per call.
const CHUNK: usize = 64 * 1024;

/// Pulls [`SessionRecord`]s out of a growing byte source (a `File` opened on
/// `events.ndjson`, kept open to follow it).
#[derive(Debug)]
pub struct RecordReader<R> {
    inner: R,
    buf: Vec<u8>,
    /// Start of the first unconsumed byte in `buf`.
    start: usize,
    /// How far past `start` has been searched for a newline, so a long partial
    /// line is not rescanned on every call.
    scanned: usize,
    skipped: u64,
}

impl<R: Read> RecordReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            start: 0,
            scanned: 0,
            skipped: 0,
        }
    }

    /// The next complete record, or `None` when the source has nothing more
    /// right now (a trailing partial line stays buffered for the next call).
    pub fn next_record(&mut self) -> io::Result<Option<SessionRecord>> {
        loop {
            let newline = self
                .buf
                .get(self.scanned..)
                .and_then(|rest| rest.iter().position(|&b| b == b'\n'));
            if let Some(rel) = newline {
                let end = self.scanned + rel;
                let decoded = self.buf.get(self.start..end).map(decode_line);
                self.start = end + 1;
                self.scanned = self.start;
                match decoded {
                    Some(Decoded::Record(r)) => return Ok(Some(r)),
                    Some(Decoded::Empty) | None => {}
                    Some(Decoded::Bad) => self.skipped += 1,
                }
                continue;
            }
            self.scanned = self.buf.len();
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    /// Complete lines skipped because they did not decode.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Bytes of a trailing partial line held back, waiting for its newline.
    pub fn partial_len(&self) -> usize {
        self.buf.len().saturating_sub(self.start)
    }

    /// Read one chunk. `false` at the source's current end.
    fn fill(&mut self) -> io::Result<bool> {
        // Drop consumed bytes before growing, so the buffer holds at most the
        // partial line plus one chunk.
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.scanned = self.scanned.saturating_sub(self.start);
            self.start = 0;
        }
        let old = self.buf.len();
        self.buf.resize(old + CHUNK, 0);
        let n = loop {
            let Some(dst) = self.buf.get_mut(old..) else {
                break 0;
            };
            match self.inner.read(dst) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.buf.truncate(old);
                    return Err(e);
                }
            }
        };
        self.buf.truncate(old + n);
        Ok(n > 0)
    }
}

enum Decoded {
    Record(SessionRecord),
    /// Blank, or a record that carries neither key.
    Empty,
    Bad,
}

fn decode_line(line: &[u8]) -> Decoded {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Decoded::Empty;
    }
    match serde_json::from_slice::<SessionRecord>(line) {
        Ok(r) if r.kind.is_none() && r.session.is_none() => Decoded::Empty,
        Ok(r) => Decoded::Record(r),
        Err(_) => Decoded::Bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::session::format::SessionRecordKind;
    use std::io::Write;

    fn all(reader: &mut RecordReader<std::fs::File>) -> Vec<SessionRecord> {
        std::iter::from_fn(|| reader.next_record().expect("read")).collect()
    }

    /// I4: a line still being written is not decoded early, and is read whole
    /// once its newline lands — following the same open file.
    #[test]
    fn reader_waits_for_a_torn_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.ndjson");
        let mut w = std::fs::File::create(&path).expect("create");
        w.write_all(b"{\"at_unix_ms\":1,\"kind\":{\"type\":\"ResultStart\",\"addr\":\"//a:b\"}}\n{\"at_unix_ms\":2,\"kind\":{\"type\":\"Result")
            .expect("write");
        let mut r = RecordReader::new(std::fs::File::open(&path).expect("open"));
        let first = all(&mut r);
        assert_eq!(first.len(), 1, "only the complete line");
        assert!(r.partial_len() > 0, "the torn line is held back");
        assert_eq!(r.skipped(), 0, "a torn line is not a bad line");

        w.write_all(b"End\",\"addr\":\"//a:b\",\"error\":null}}\n")
            .expect("finish");
        let second = all(&mut r);
        assert_eq!(second.len(), 1);
        assert_eq!(second.first().map(|r| r.at_unix_ms), Some(2));
        assert_eq!(r.partial_len(), 0);
    }

    /// A5: a complete line that does not decode is skipped and counted, and the
    /// lines after it are still read. A line with neither key is skipped
    /// without counting.
    #[test]
    fn undecodable_line_is_skipped_and_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.ndjson");
        std::fs::write(
            &path,
            concat!(
                "{\"at_unix_ms\":1,\"session\":{\"type\":\"Truncated\"}}\n",
                "not json at all\n",
                "{\"at_unix_ms\":2,\"kind\":{\"type\":\"ResultStart\"}}\n",
                "\n",
                "{\"at_unix_ms\":3}\n",
                "\u{1}\u{2}\u{ff}\n",
                "{\"at_unix_ms\":4,\"session\":{\"type\":\"End\",\"exit_code\":0}}\n",
            ),
        )
        .expect("write");
        let mut r = RecordReader::new(std::fs::File::open(&path).expect("open"));
        let records = all(&mut r);
        let stamps: Vec<u64> = records.iter().map(|r| r.at_unix_ms).collect();
        assert_eq!(stamps, vec![1, 4], "reading went past every bad line");
        assert_eq!(
            r.skipped(),
            3,
            "garbage, a known kind missing its field, control bytes"
        );
        assert_eq!(
            records.last().and_then(|r| r.session.clone()),
            Some(SessionRecordKind::End {
                exit_code: Some(0),
                panicked: false
            })
        );
    }

    /// No truncating cap: a record far longer than a read chunk is one record.
    #[test]
    fn a_line_over_one_mib_is_parsed() {
        let msg = "x".repeat(3 * 1024 * 1024);
        let line = serde_json::to_string(&SessionRecord::session(
            9,
            SessionRecordKind::Log {
                level: "INFO".into(),
                msg: msg.clone(),
            },
        ))
        .expect("encode");
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let mut r = RecordReader::new(std::io::Cursor::new(bytes));
        let rec = r.next_record().expect("read").expect("one record");
        assert_eq!(
            rec.session,
            Some(SessionRecordKind::Log {
                level: "INFO".into(),
                msg
            })
        );
        assert!(r.next_record().expect("read").is_none());
    }
}
