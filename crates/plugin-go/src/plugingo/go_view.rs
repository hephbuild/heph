//! The bytes of a `.go` file that `go/build` reads: a port of `readGoInfo`
//! (`$GOROOT/src/go/build/read.go`, verified against
//! [`crate::plugingo::nongo_lanes::GO_HEADER_RULES_VERIFIED_AGAINST`]).
//!
//! `go list` learns everything it reports about a `.go` file from two things:
//!
//! - the **header**: leading comments, the package clause and the import
//!   declarations (with their doc comments, so the cgo preamble), up to the end
//!   of the imports. Build constraints, `//go:debug` and other directives,
//!   `// import` comments and the imports themselves all live there;
//! - if the file imports `"embed"`, the **`//go:embed` comments** of the rest of
//!   the file, found by scanning it with `go/scanner` — with their positions,
//!   which reach `go list`'s `Error` when a pattern matches nothing. `//line`
//!   directives move those positions, so they are part of the view too.
//!
//! [`extract_go_view`] returns an encoding of exactly that, so `_golist` can key
//! on it instead of on the file: editing a function body moves nothing, editing
//! an import, a constraint, the preamble or an embed pattern moves the key.
//!
//! The encoding is a tag byte, then:
//!
//! - `H` + the header — the file has no `"embed"` import (or `go/build` stopped
//!   on a NUL, which it reports as an error naming only the file);
//! - `W` + the whole file — the header has a syntax error, and `readGoInfo`
//!   then hands the whole file to `go/parser`;
//! - `E` + the header's length (u64 LE) + the header + one record per comment
//!   that is a `//go:embed` or a line directive: line, column, length (u64 LE
//!   each) and its raw bytes.
//!
//! The header keeps a leading UTF-8 BOM (it was read), and is otherwise exactly
//! `info.header`: when the reader stopped cleanly, the byte that told it to stop
//! is not part of it.
//!
//! The same [`ImportReader`] serves the C/asm-family files, whose view is what
//! `readComments` reads ([`extract_go_build_header`]): one port of Go's
//! `importReader`, not two.
//!
//! # Scope of the trust rule
//!
//! `_golist` keys a `.go` file on this view, not on its bytes, on the rule that
//! `go list`, as `driver_golist` invokes it, learns nothing else of the file.
//! That rule is a property of the Go toolchain's source, so it holds for the
//! versions it was checked against: Go
//! [`crate::plugingo::nongo_lanes::GO_HEADER_RULES_VERIFIED_AGAINST`]. It is
//! applied to every Go version (no version gate); on each Go bump, re-check
//! `go/build/read.go` (`readGoInfo`, `readComments`) and `cmd/go`'s module index
//! (`internal/modindex`, which caches the same reads) before moving that
//! constant. A field or flag added to `go list`'s invocation
//! (`driver_golist::GOLIST_JSON_FIELDS`) can break it too.

use std::io::{BufRead, Read};

/// How much of a file is read per `read` call. Without an `"embed"` import the
/// extractor stops at the end of the imports, so it reads at most the header
/// plus one chunk of any file.
pub(crate) const READ_CHUNK: usize = 4096;

/// The UTF-8 byte-order mark `go/build` skips at the start of a file.
const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

/// See the module docs.
pub fn extract_go_view(r: impl Read) -> std::io::Result<Vec<u8>> {
    extract_with_chunk(r, READ_CHUNK)
}

/// The prefix of a C/asm-family file that `go/build` reads: the bytes
/// `readComments` (`$GOROOT/src/go/build/read.go`) consumes, verbatim — a
/// leading UTF-8 BOM, then whitespace, `;`, `//` and `/* */` comments, up to and
/// including the first byte that ends them (the first code byte, the byte after
/// a stray `/`, a NUL and the byte after it). Everything `go/build` concludes
/// about the file (its build constraints, and whether reading it failed) is a
/// function of these bytes, so hashing them is hashing what `go list` saw.
///
/// Streams: reads at most the returned length plus one [`READ_CHUNK`], however
/// large the file. An I/O error is returned, not folded into the prefix.
pub fn extract_go_build_header(r: impl Read) -> std::io::Result<Vec<u8>> {
    extract_comments_with_chunk(r, READ_CHUNK)
}

/// [`extract_go_build_header`] with a given read size: `readComments` is
/// `peekByte(true)` once, keeping every byte it read.
pub(crate) fn extract_comments_with_chunk(r: impl Read, chunk: usize) -> std::io::Result<Vec<u8>> {
    let mut src = std::io::BufReader::with_capacity(chunk, r);
    let (bom, pending) = take_bom(&mut src)?;
    let mut rd = ImportReader::new(&mut src, pending);
    rd.peek_byte(true)?;
    let mut out = Vec::with_capacity(BOM.len() + rd.buf.len());
    if bom {
        out.extend_from_slice(&BOM);
    }
    out.append(&mut rd.buf);
    Ok(out)
}

/// `newImportReader`'s BOM step: a leading UTF-8 BOM is discarded before
/// scanning (it is still part of what was read, so callers keep it in their
/// output). Returns whether there was one, and the bytes read ahead that turned
/// out not to be one — the start of the scan.
fn take_bom(src: &mut impl BufRead) -> std::io::Result<(bool, Vec<u8>)> {
    let lead = src.fill_buf()?;
    if lead.starts_with(&BOM) {
        src.consume(BOM.len());
        return Ok((true, Vec::new()));
    }
    if lead.len() < BOM.len() && BOM.starts_with(lead) && !lead.is_empty() {
        // A short read that could still be a BOM: settle it byte by byte.
        let mut peeked = Vec::with_capacity(BOM.len());
        while peeked.len() < BOM.len() {
            let Some(b) = next_src_byte(src)? else { break };
            peeked.push(b);
            if !BOM.starts_with(&peeked) {
                break;
            }
        }
        if peeked == BOM {
            return Ok((true, Vec::new()));
        }
        return Ok((false, peeked));
    }
    Ok((false, Vec::new()))
}

/// One byte from `src`, `None` at EOF.
fn next_src_byte(src: &mut impl BufRead) -> std::io::Result<Option<u8>> {
    let buf = loop {
        match src.fill_buf() {
            Ok(b) => break b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    };
    let Some(&b) = buf.first() else {
        return Ok(None);
    };
    src.consume(1);
    Ok(Some(b))
}

fn extract_with_chunk(r: impl Read, chunk: usize) -> std::io::Result<Vec<u8>> {
    let mut src = std::io::BufReader::with_capacity(chunk, r);
    let (bom, pending) = take_bom(&mut src)?;
    let mut rd = ImportReader::new(&mut src, pending);
    rd.read_go_info()?;
    let bom: &[u8] = if bom { &BOM } else { &[] };

    let mut out = Vec::new();
    match rd.err {
        Stop::Syntax => {
            // "If we stopped for a syntax error, consume the whole file so that
            // we are sure we don't change the errors that go/parser returns."
            rd.read_rest()?;
            out.push(b'W');
            out.extend_from_slice(bom);
            out.extend_from_slice(&rd.buf);
        }
        Stop::Nul => {
            out.push(b'H');
            out.extend_from_slice(bom);
            out.extend_from_slice(&rd.buf);
        }
        Stop::None => {
            let header_len = if rd.eof {
                rd.buf.len()
            } else {
                // The byte that told the reader the imports were over.
                rd.buf.len().saturating_sub(1)
            };
            if !rd.has_embed {
                out.push(b'H');
                out.extend_from_slice(bom);
                out.extend_from_slice(rd.buf.get(..header_len).unwrap_or(&rd.buf));
            } else {
                rd.read_rest()?;
                out.push(b'E');
                let header = rd.buf.get(..header_len).unwrap_or(&rd.buf);
                put_u64(&mut out, (bom.len() + header.len()) as u64);
                out.extend_from_slice(bom);
                out.extend_from_slice(header);
                for c in view_comments(&rd.buf) {
                    put_u64(&mut out, c.line as u64);
                    put_u64(&mut out, c.col as u64);
                    put_u64(&mut out, c.text.len() as u64);
                    out.extend_from_slice(c.text);
                }
            }
        }
    }
    Ok(out)
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Why the import reader stopped early. Go's `errSyntax` and `errNUL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    None,
    Syntax,
    Nul,
}

/// `go/build.importReader`, over a streaming source. `buf` is every byte read
/// after the BOM, exactly as Go's `r.buf`.
struct ImportReader<'a, R> {
    src: &'a mut R,
    /// Bytes read ahead of `src` (a would-be BOM that was not one).
    pending: std::vec::IntoIter<u8>,
    buf: Vec<u8>,
    /// Go's `r.peek`: the last byte read and not yet consumed, 0 for none.
    peek: u8,
    err: Stop,
    eof: bool,
    /// Whether an import spec may name `"embed"` (see [`may_be_embed`]).
    has_embed: bool,
}

/// `isIdent`.
fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

impl<'a, R: BufRead> ImportReader<'a, R> {
    fn new(src: &'a mut R, pending: Vec<u8>) -> Self {
        Self {
            src,
            pending: pending.into_iter(),
            buf: Vec::new(),
            peek: 0,
            err: Stop::None,
            eof: false,
            has_embed: false,
        }
    }

    fn syntax_error(&mut self) {
        if self.err == Stop::None {
            self.err = Stop::Syntax;
        }
    }

    /// `readByte`: read one byte, record it, flag NUL and EOF; 0 for either.
    fn read_byte(&mut self) -> std::io::Result<u8> {
        let next = match self.pending.next() {
            Some(b) => Some(b),
            None => next_src_byte(self.src)?,
        };
        match next {
            Some(c) => {
                self.buf.push(c);
                if c == 0 && self.err == Stop::None {
                    self.err = Stop::Nul;
                }
                Ok(c)
            }
            None => {
                self.eof = true;
                Ok(0)
            }
        }
    }

    /// `readRest`: the rest of the file into `buf`.
    fn read_rest(&mut self) -> std::io::Result<()> {
        self.buf.extend(self.pending.by_ref());
        self.src.read_to_end(&mut self.buf)?;
        self.eof = true;
        Ok(())
    }

    /// `peekByte`.
    fn peek_byte(&mut self, skip_space: bool) -> std::io::Result<u8> {
        if self.err != Stop::None {
            return Ok(0);
        }
        let mut c = self.peek;
        if c == 0 {
            c = self.read_byte()?;
        }
        while self.err == Stop::None && !self.eof {
            if skip_space {
                match c {
                    b' ' | b'\x0c' | b'\t' | b'\r' | b'\n' | b';' => {
                        c = self.read_byte()?;
                        continue;
                    }
                    b'/' => {
                        c = self.read_byte()?;
                        if c == b'/' {
                            while c != b'\n' && self.err == Stop::None && !self.eof {
                                c = self.read_byte()?;
                            }
                        } else if c == b'*' {
                            let mut c1 = 0u8;
                            while (c != b'*' || c1 != b'/') && self.err == Stop::None {
                                if self.eof {
                                    self.syntax_error();
                                }
                                c = c1;
                                c1 = self.read_byte()?;
                            }
                        } else {
                            self.syntax_error();
                        }
                        c = self.read_byte()?;
                        continue;
                    }
                    _ => {}
                }
            }
            break;
        }
        self.peek = c;
        Ok(c)
    }

    /// `nextByte`.
    fn next_byte(&mut self, skip_space: bool) -> std::io::Result<u8> {
        let c = self.peek_byte(skip_space)?;
        self.peek = 0;
        Ok(c)
    }

    /// `readKeyword`.
    fn read_keyword(&mut self, kw: &[u8]) -> std::io::Result<()> {
        self.peek_byte(true)?;
        for &k in kw {
            if self.next_byte(false)? != k {
                self.syntax_error();
                return Ok(());
            }
        }
        if is_ident(self.peek_byte(false)?) {
            self.syntax_error();
        }
        Ok(())
    }

    /// `readIdent`.
    fn read_ident(&mut self) -> std::io::Result<()> {
        let c = self.peek_byte(true)?;
        if !is_ident(c) {
            self.syntax_error();
            return Ok(());
        }
        while is_ident(self.peek_byte(false)?) {
            self.peek = 0;
        }
        Ok(())
    }

    /// `readString`, noting whether the literal may be `"embed"`.
    fn read_string(&mut self) -> std::io::Result<()> {
        let quote = self.next_byte(true)?;
        // The quote is the last byte read: `peekByte` leaves `peek` at it.
        let start = self.buf.len().saturating_sub(1);
        match quote {
            b'`' => {
                while self.err == Stop::None {
                    if self.next_byte(false)? == b'`' {
                        break;
                    }
                    if self.eof {
                        self.syntax_error();
                    }
                }
            }
            b'"' => {
                while self.err == Stop::None {
                    let c = self.next_byte(false)?;
                    if c == b'"' {
                        break;
                    }
                    if self.eof || c == b'\n' {
                        self.syntax_error();
                    }
                    if c == b'\\' {
                        self.next_byte(false)?;
                    }
                }
            }
            _ => self.syntax_error(),
        }
        if self.err == Stop::None
            && let Some(lit) = self.buf.get(start..)
            && may_be_embed(lit)
        {
            self.has_embed = true;
        }
        Ok(())
    }

    /// `readImport`.
    fn read_import(&mut self) -> std::io::Result<()> {
        let c = self.peek_byte(true)?;
        if c == b'.' {
            self.peek = 0;
        } else if is_ident(c) {
            self.read_ident()?;
        }
        self.read_string()
    }

    /// The import-reading half of `readGoInfo`.
    fn read_go_info(&mut self) -> std::io::Result<()> {
        self.read_keyword(b"package")?;
        self.read_ident()?;
        while self.peek_byte(true)? == b'i' {
            self.read_keyword(b"import")?;
            if self.peek_byte(true)? == b'(' {
                self.next_byte(false)?;
                while self.peek_byte(true)? != b')' && self.err == Stop::None {
                    self.read_import()?;
                }
                self.next_byte(false)?;
            } else {
                self.read_import()?;
            }
        }
        Ok(())
    }
}

/// Whether a well-formed import path literal may unquote to `embed`, as
/// `strconv.Unquote` would. A raw string loses its `\r`s; an interpreted string
/// with an escape in it is assumed to (decoding escapes here would be a second
/// `strconv`, and over-hashing such a file costs only a re-list).
fn may_be_embed(lit: &[u8]) -> bool {
    let Some(inner) = lit.get(1..lit.len().saturating_sub(1)) else {
        return false;
    };
    match lit.first() {
        Some(b'`') => inner.iter().filter(|&&b| b != b'\r').eq(b"embed".iter()),
        Some(b'"') => inner.contains(&b'\\') || inner == b"embed",
        _ => false,
    }
}

/// A comment of the view: where it starts and its raw bytes.
#[derive(Debug, PartialEq, Eq)]
struct ViewComment<'a> {
    line: usize,
    col: usize,
    text: &'a [u8],
}

/// Every comment `go/scanner` returns over `src` (the file after the BOM, as
/// `readGoInfo` scans it) that is part of the view: a `//go:embed` comment, or
/// one that may be a line directive. Positions are unadjusted, 1-based, the
/// column in bytes — `token.Position` before `//line` remapping, which the
/// directives recorded beside them determine.
///
/// The scan only has to find comments, so it tokenizes only what can hide a
/// `//`: strings, raw strings, rune literals and comments, each ended exactly
/// as `go/scanner` ends it. No other token can contain a `/`, a quote or a
/// newline, so every other byte is skipped one at a time.
fn view_comments(src: &[u8]) -> Vec<ViewComment<'_>> {
    let mut out = Vec::new();
    // `Scanner.next`: a UTF-16 BOM at the start consumes the whole input.
    // Unreachable from `extract_with_chunk` (comments are scanned only after a
    // clean header, which starts with `package`); kept so this stays a faithful
    // port should another caller appear.
    if matches!(src, [0xff, 0xfe, ..] | [0xfe, 0xff, ..]) {
        return out;
    }
    let mut i = 0;
    // Line accounting up to `pos_at`, advanced monotonically.
    let mut pos_at = 0;
    let mut line = 1;
    let mut line_start = 0;
    let at = |b: usize| src.get(b).copied();
    while let Some(c) = at(i) {
        match c {
            b'/' if matches!(at(i + 1), Some(b'/' | b'*')) => {
                let start = i;
                i += 2;
                if at(start + 1) == Some(b'/') {
                    // The final '\n' is not part of the comment.
                    while at(i).is_some_and(|ch| ch != b'\n') {
                        i += 1;
                    }
                } else {
                    while let Some(ch) = at(i) {
                        i += 1;
                        if ch == b'*' && at(i) == Some(b'/') {
                            i += 1;
                            break;
                        }
                    }
                }
                let text = src.get(start..i).unwrap_or_default();
                if is_view_comment(text) {
                    while pos_at < start {
                        if at(pos_at) == Some(b'\n') {
                            line += 1;
                            line_start = pos_at + 1;
                        }
                        pos_at += 1;
                    }
                    out.push(ViewComment {
                        line,
                        col: start - line_start + 1,
                        text,
                    });
                }
            }
            b'"' => i = scan_quoted(src, i + 1, b'"'),
            b'\'' => i = scan_quoted(src, i + 1, b'\''),
            b'`' => {
                i += 1;
                while let Some(ch) = at(i) {
                    i += 1;
                    if ch == b'`' {
                        break;
                    }
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether a comment is part of the view: `//go:embed…` once `\r`s are gone
/// (`scanComment` strips them before `readGoInfo` tests the prefix), or a
/// possible `//line` / `/*line` directive (tested on the bytes either way).
fn is_view_comment(text: &[u8]) -> bool {
    starts_with_without_cr(text, 0, b"//go:embed")
        || text.get(2..).is_some_and(|r| r.starts_with(b"line "))
        || starts_with_without_cr(text, 2, b"line ")
}

/// Whether `text` with its `\r`s removed, then its first `skip` bytes, starts
/// with `prefix` — without building the stripped copy.
fn starts_with_without_cr(text: &[u8], skip: usize, prefix: &[u8]) -> bool {
    let mut rest = text.iter().copied().filter(|&b| b != b'\r').skip(skip);
    prefix.iter().all(|&p| rest.next() == Some(p))
}

/// `scanString` / `scanRune` from just after the opening `quote`: the index
/// after the literal. Ends at the closing quote (consumed), or before a newline
/// or at EOF (unterminated).
fn scan_quoted(src: &[u8], mut i: usize, quote: u8) -> usize {
    while let Some(&ch) = src.get(i) {
        if ch == b'\n' {
            break;
        }
        i += 1;
        if ch == quote {
            break;
        }
        if ch == b'\\' {
            i = scan_escape(src, i, quote);
        }
    }
    i
}

/// `scanEscape` at the byte after a `\`: the index after the escape. Stops at
/// the offending byte, unconsumed, on a malformed one.
fn scan_escape(src: &[u8], mut i: usize, quote: u8) -> usize {
    let Some(&c) = src.get(i) else { return i };
    let (digits, base) = match c {
        b'a' | b'b' | b'f' | b'n' | b'r' | b't' | b'v' | b'\\' => return i + 1,
        _ if c == quote => return i + 1,
        b'0'..=b'7' => (3, 8),
        b'x' => {
            i += 1;
            (2, 16)
        }
        b'u' => {
            i += 1;
            (4, 16)
        }
        b'U' => {
            i += 1;
            (8, 16)
        }
        _ => return i,
    };
    for _ in 0..digits {
        match src.get(i).and_then(|&d| (d as char).to_digit(16)) {
            Some(d) if d < base => i += 1,
            _ => break,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(input: &[u8]) -> Vec<u8> {
        extract_go_view(input).expect("in-memory read cannot fail")
    }

    fn header(tag: u8, bytes: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        v.extend_from_slice(bytes);
        v
    }

    /// The embed directives of a view, as (line, col, text).
    fn embeds(input: &[u8]) -> Vec<(usize, usize, String)> {
        let v = view(input);
        assert_eq!(
            v.first(),
            Some(&b'E'),
            "{:?}",
            String::from_utf8_lossy(input)
        );
        let rd = |at: usize| u64::from_le_bytes(v[at..at + 8].try_into().unwrap()) as usize;
        let hlen = rd(1);
        let mut i = 9 + hlen;
        let mut out = Vec::new();
        while i < v.len() {
            let (line, col, len) = (rd(i), rd(i + 8), rd(i + 16));
            i += 24;
            out.push((
                line,
                col,
                String::from_utf8_lossy(&v[i..i + len]).into_owned(),
            ));
            i += len;
        }
        out
    }

    const EMBED_FILE: &str = "package p\n\nimport _ \"embed\"\n\n//go:embed a.txt\nvar s string\n";

    /// The adversarial table: what is and is not part of a `.go` file's view.
    #[test]
    fn go_view_adversarial_table() {
        let cases: &[(&str, &[u8], Vec<u8>)] = &[
            ("empty file", b"", header(b'W', b"")),
            ("package only, EOF", b"package p", header(b'H', b"package p")),
            (
                "no imports: the first decl ends the header",
                b"package p\n\nfunc f() {}\n",
                header(b'H', b"package p\n\n"),
            ),
            (
                "single import",
                b"package p\nimport \"fmt\"\nfunc f() { fmt.Println() }\n",
                header(b'H', b"package p\nimport \"fmt\"\n"),
            ),
            (
                "grouped imports",
                b"package p\nimport (\n\t\"fmt\"\n\tx \"os\"\n\t. \"io\"\n)\nvar v = 1\n",
                header(b'H', b"package p\nimport (\n\t\"fmt\"\n\tx \"os\"\n\t. \"io\"\n)\n"),
            ),
            (
                "an import inside a function body is not part of the view",
                b"package p\nfunc f() {\nimport \"os\"\n}\n",
                header(b'H', b"package p\n"),
            ),
            (
                "constraint before the package clause",
                b"//go:build linux\n\npackage p\nvar v int\n",
                header(b'H', b"//go:build linux\n\npackage p\n"),
            ),
            (
                // go/build reads it (it is in the header) but it is not a
                // constraint; after the first decl it is not even read.
                "a build tag after the package clause",
                b"package p\n//go:build linux\nvar v int\n//go:build windows\n",
                header(b'H', b"package p\n//go:build linux\n"),
            ),
            (
                "BOM",
                b"\xef\xbb\xbfpackage p\nvar v int\n",
                header(b'H', b"\xef\xbb\xbfpackage p\n"),
            ),
            (
                "CRLF",
                b"package p\r\nimport \"fmt\"\r\nvar v int\r\n",
                header(b'H', b"package p\r\nimport \"fmt\"\r\n"),
            ),
            (
                "cgo preamble with #cgo lines",
                b"package p\n\n// #cgo LDFLAGS: -lm\n// #include <math.h>\nimport \"C\"\n\nfunc f() {}\n",
                header(
                    b'H',
                    b"package p\n\n// #cgo LDFLAGS: -lm\n// #include <math.h>\nimport \"C\"\n\n",
                ),
            ),
            (
                "block-comment preamble",
                b"package p\n/*\n#cgo CFLAGS: -O2\n*/\nimport \"C\"\nvar v int\n",
                header(b'H', b"package p\n/*\n#cgo CFLAGS: -O2\n*/\nimport \"C\"\n"),
            ),
            (
                "non-ASCII identifiers and comments",
                "// héllo\npackage pä\nimport ü \"fmt\"\nvar v int\n".as_bytes(),
                header(b'H', "// héllo\npackage pä\nimport ü \"fmt\"\n".as_bytes()),
            ),
            (
                "a syntax error in the header takes the whole file",
                b"package p\nimport fmt\nfunc f() {}\n",
                header(b'W', b"package p\nimport fmt\nfunc f() {}\n"),
            ),
            (
                "NUL stops the read",
                b"package p\n\0import \"fmt\"\n",
                header(b'H', b"package p\n\0"),
            ),
            (
                "//go:embed without an embed import is not part of the view",
                b"package p\nimport \"fmt\"\nvar x int\n//go:embed a.txt\nvar s string\n",
                header(b'H', b"package p\nimport \"fmt\"\n"),
            ),
            (
                "an import named like embed is not embed",
                b"package p\nimport \"example.com/embed\"\nvar x int\n//go:embed a.txt\nvar s string\n",
                header(b'H', b"package p\nimport \"example.com/embed\"\n"),
            ),
            (
                // The import reader skips comments as space, so one right
                // after the imports is read as part of the header.
                "a comment right after the imports is in the header",
                b"package p\nimport \"fmt\"\n// doc\nvar x int\n",
                header(b'H', b"package p\nimport \"fmt\"\n// doc\n"),
            ),
        ];
        for (what, input, want) in cases {
            assert_eq!(
                &view(input),
                want,
                "{what}: {:?}",
                String::from_utf8_lossy(input)
            );
        }

        // What the extractor returns on this corpus plus an embed file, pinned
        // next to the format version that keys every cached header tar (as the
        // C/asm extractor's `extractor_adversarial_table` is). A change that
        // moves this digest must bump `GO_HEADERS_FORMAT_VERSION` (and update
        // both here, and there); otherwise old-shape tars are still served.
        let mut h = xxhash_rust::xxh3::Xxh3Default::new();
        let corpus = cases
            .iter()
            .map(|(_, input, _)| *input)
            .chain([EMBED_FILE.as_bytes()]);
        for input in corpus {
            let out = view(input);
            h.update(&(out.len() as u64).to_le_bytes());
            h.update(&out);
        }
        assert_eq!(
            (
                crate::plugingo::driver_headers::GO_HEADERS_FORMAT_VERSION,
                format!("{:016x}", h.digest())
            ),
            (2, "74fe229785b0c2c8".to_string()),
            "the .go view extractor's output moved: bump GO_HEADERS_FORMAT_VERSION and re-pin"
        );
    }

    /// Embed directives are found the way `go/scanner` finds comments: never
    /// inside a string, raw string, rune literal or block comment.
    #[test]
    fn embed_scan_skips_literals_and_block_comments() {
        let src = "package p\n\nimport \"embed\"\n\n\
                   var a = \"//go:embed in_string\"\n\
                   var b = `\n//go:embed in_raw\n`\n\
                   var c = '\\''\n\
                   /* //go:embed in_block\n//go:embed in_block2 */\n\
                   var d = \"esc \\\" //go:embed in_escaped\"\n\
                   //go:embed real.txt\n\
                   var e embed.FS\n";
        assert_eq!(
            embeds(src.as_bytes()),
            vec![(13, 1, "//go:embed real.txt".to_string())]
        );
    }

    #[test]
    fn embed_import_forms() {
        for imp in [
            "import \"embed\"",
            "import _ \"embed\"",
            "import (\n\t\"fmt\"\n\t_ \"embed\"\n)",
            "import `embed`",
            "import `em\rbed`",
            "import \"\\x65mbed\"",
        ] {
            let src = format!("package p\n{imp}\n//go:embed x\nvar v string\n");
            assert_eq!(embeds(src.as_bytes()).len(), 1, "{imp:?} may import embed");
        }
    }

    /// The point of the view: a body edit leaves it alone, an import edit or an
    /// embed-pattern edit does not.
    #[test]
    fn body_edits_keep_the_view_and_header_edits_move_it() {
        let base = "package p\n\nimport \"fmt\"\n\nfunc F() { fmt.Println(1) }\n";
        assert_eq!(
            view(base.as_bytes()),
            view(base.replace("Println(1)", "Println(2)").as_bytes()),
            "body edit"
        );
        assert_eq!(
            view(base.as_bytes()),
            view(format!("{base}\nfunc G() {{}}\n").as_bytes()),
            "a new function"
        );
        assert_ne!(
            view(base.as_bytes()),
            view(base.replace("\"fmt\"", "\"os\"").as_bytes()),
            "import edit"
        );

        // With embed: a body edit below the directive keeps it; one above it
        // shifts its position (go list reports that position), so it moves.
        let with_body = format!("{EMBED_FILE}\nfunc F() int {{ return 1 }}\n");
        assert_eq!(
            view(with_body.as_bytes()),
            view(with_body.replace("return 1", "return 2").as_bytes())
        );
        assert_ne!(
            view(EMBED_FILE.as_bytes()),
            view(EMBED_FILE.replace("a.txt", "b.txt").as_bytes()),
            "embed pattern edit"
        );
        let above = EMBED_FILE.replace("//go:embed", "var x = 1\n\n//go:embed");
        assert_ne!(view(EMBED_FILE.as_bytes()), view(above.as_bytes()));
        assert_eq!(
            embeds(EMBED_FILE.as_bytes()),
            vec![(5, 1, "//go:embed a.txt".to_string())]
        );
    }

    #[test]
    fn line_directives_are_recorded() {
        let src = format!("{EMBED_FILE}//line other.go:10\n/*line x.go:1:1*/\n");
        let e = embeds(src.as_bytes());
        assert_eq!(e.len(), 3, "{e:?}");
        assert_eq!(e[1], (7, 1, "//line other.go:10".to_string()));
    }

    #[test]
    fn crlf_embed_directive_is_found() {
        let src = "package p\r\nimport \"embed\"\r\n//go:em\rbed a\r\nvar f embed.FS\r\n";
        assert_eq!(embeds(src.as_bytes()).len(), 1);
    }

    #[test]
    fn view_is_independent_of_read_boundaries() {
        let inputs: Vec<Vec<u8>> = vec![
            EMBED_FILE.as_bytes().to_vec(),
            b"\xef\xbb\xbfpackage p\nimport \"fmt\"\nvar v int\n".to_vec(),
            b"\xef\xbbpackage p\n".to_vec(),
            b"package p\nimport fmt\n".to_vec(),
        ];
        for input in inputs {
            assert_eq!(
                extract_with_chunk(input.as_slice(), 1).unwrap(),
                view(&input),
                "{:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }

    const EMBED_HEAD: &str = "package p\nimport \"embed\"\n";

    /// Rune literals end where `go/scanner` ends them, so a quote or backslash
    /// inside one does not swallow the directive after it.
    #[test]
    fn rune_literals_do_not_hide_a_directive() {
        for rune in ["'\"'", "'\\\\'", "'\\x41'", "'\\''", "'ሴ'", "'\\u1234'"] {
            let src = format!("{EMBED_HEAD}var r = {rune}\n//go:embed x\nvar s string\n");
            assert_eq!(
                embeds(src.as_bytes()),
                vec![(4, 1, "//go:embed x".to_string())],
                "{rune}"
            );
        }
    }

    /// A block comment ends at its first `*/` (no nesting): the line comment
    /// after it is a real directive.
    #[test]
    fn block_comments_do_not_nest() {
        let src = format!("{EMBED_HEAD}/* /* */ //go:embed x\nvar s string\n");
        assert_eq!(
            embeds(src.as_bytes()),
            vec![(3, 10, "//go:embed x".to_string())]
        );
    }

    /// A literal or block comment left open at EOF ends the scan: no panic, and
    /// the directive inside it is not one.
    #[test]
    fn unterminated_tokens_at_eof() {
        for tail in [
            "var s = \"abc //go:embed x",
            "var s = `abc\n//go:embed x\n",
            // After a decl: right after the imports the import reader would
            // read it as header space, and hit EOF in it (a syntax error).
            "var x int\n/* //go:embed x\n",
            "var r = '\\",
            "var s = \"\\x",
            "var x = 1 /",
        ] {
            let src = format!("{EMBED_HEAD}{tail}");
            assert_eq!(embeds(src.as_bytes()), vec![], "{tail:?}");
        }
    }

    /// The header keeps the BOM, the positions do not count it (`readGoInfo`
    /// scans the buffer after it).
    #[test]
    fn bom_with_embed() {
        let src = format!("\u{feff}{EMBED_FILE}");
        let v = view(src.as_bytes());
        let hlen = u64::from_le_bytes(v[1..9].try_into().unwrap()) as usize;
        assert!(
            v[9..9 + hlen].starts_with(b"\xef\xbb\xbfpackage p"),
            "{v:?}"
        );
        assert_eq!(
            embeds(src.as_bytes()),
            vec![(5, 1, "//go:embed a.txt".to_string())]
        );
    }

    /// A NUL past the imports of an embed-importing file is an illegal
    /// character to `go/scanner`, which scans on: a directive after it counts.
    #[test]
    fn nul_in_the_body_of_an_embed_file() {
        let src = format!("{EMBED_HEAD}var x = 1\0\n//go:embed x\nvar s string\n");
        assert_eq!(
            embeds(src.as_bytes()),
            vec![(4, 1, "//go:embed x".to_string())]
        );
    }

    /// A UTF-16 BOM is no package clause: a header syntax error, the whole file.
    #[test]
    fn utf16_bom_is_a_header_syntax_error() {
        for bom in [&b"\xff\xfe"[..], b"\xfe\xff"] {
            let mut src = bom.to_vec();
            src.extend_from_slice(EMBED_FILE.as_bytes());
            assert_eq!(view(&src), header(b'W', &src));
        }
    }

    /// An unterminated block comment in the header is a syntax error: the whole
    /// file, so a body edit then moves the view (go/parser reads it all).
    #[test]
    fn unterminated_block_comment_in_the_header_takes_the_whole_file() {
        let src = b"package p\n/* never closed\nfunc f() {}\n";
        assert_eq!(view(src), header(b'W', src));
    }

    /// Columns are bytes, as in `token.Position`: a directive after non-ASCII on
    /// its line sits at its byte offset.
    #[test]
    fn columns_count_bytes() {
        let line = "var s = \"é\" //go:embed x";
        let src = format!("{EMBED_HEAD}{line}\n");
        let col = line.find("//go:embed").unwrap() + 1;
        assert_eq!(col, 14, "é is two bytes");
        assert_eq!(
            embeds(src.as_bytes()),
            vec![(3, col, "//go:embed x".to_string())]
        );
    }

    /// CRLF: a `\r` is a byte of the line, not a line break; the comment keeps
    /// its raw bytes.
    #[test]
    fn crlf_positions() {
        let src = "package p\r\nimport \"embed\"\r\n\r\n  //go:embed a\r\nvar f embed.FS\r\n";
        assert_eq!(
            embeds(src.as_bytes()),
            vec![(4, 3, "//go:embed a\r".to_string())]
        );
    }

    /// Large files: an embed file's directive past many chunks is found with its
    /// line; a header syntax error takes every byte.
    #[test]
    fn large_embed_and_whole_files() {
        let filler = "func f() { _ = \"//go:embed nope\" }\n".repeat(30_000);
        let src = format!("{EMBED_FILE}{filler}//go:embed late\nvar t string\n");
        let e = embeds(src.as_bytes());
        assert_eq!(
            e.last(),
            Some(&(7 + 30_000, 1, "//go:embed late".to_string()))
        );
        assert_eq!(e.len(), 2);

        let src = format!("package p\nimport fmt\n{filler}");
        assert_eq!(view(src.as_bytes()), header(b'W', src.as_bytes()));
    }

    /// Without an embed import, only the header (plus one chunk) is read of a
    /// large file.
    #[test]
    fn reads_only_the_header_without_embed() {
        struct Counting<'a>(&'a [u8], usize);
        impl Read for Counting<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.read(buf)?;
                self.1 += n;
                Ok(n)
            }
        }
        let mut big = b"package p\nimport \"fmt\"\n".to_vec();
        big.resize(4 << 20, b'z');
        let mut r = Counting(&big, 0);
        let _ = extract_go_view(&mut r).unwrap();
        assert!(r.1 <= 2 * READ_CHUNK, "read {} bytes", r.1);
    }
}
