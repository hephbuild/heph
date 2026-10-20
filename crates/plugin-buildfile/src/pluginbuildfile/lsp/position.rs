//! Position encoding: the one place an editor's `(line, character)` becomes a
//! byte offset, and back.
//!
//! The client and server agree on what a `character` counts during
//! `initialize` (`positionEncoding`): UTF-8 bytes, UTF-16 code units (the LSP
//! default, and what this falls back to), or UTF-32 code points. Everything
//! inside the server works in byte offsets; this module converts at the
//! boundary and nowhere else.
//!
//! Lines end at `\n`, `\r\n` or a lone `\r`, as the LSP specification defines
//! them. A position past the end of its line, or past the last line, has no
//! offset: the caller treats it as "not a site", never as an error.

use lsp_types::{Position, PositionEncodingKind};

/// What a `character` in a [`Position`] counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    /// The first encoding in the client's preference list that the server
    /// supports; UTF-16 when the client offers none, or none it supports.
    pub(crate) fn negotiate(offered: Option<&[PositionEncodingKind]>) -> Encoding {
        offered
            .into_iter()
            .flatten()
            .find_map(|kind| match kind.as_str() {
                "utf-8" => Some(Encoding::Utf8),
                "utf-16" => Some(Encoding::Utf16),
                "utf-32" => Some(Encoding::Utf32),
                _ => None,
            })
            .unwrap_or(Encoding::Utf16)
    }

    pub(crate) fn kind(self) -> PositionEncodingKind {
        match self {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
            Encoding::Utf32 => PositionEncodingKind::UTF32,
        }
    }

    fn units(self, c: char) -> u32 {
        match self {
            Encoding::Utf8 => c.len_utf8() as u32,
            Encoding::Utf16 => c.len_utf16() as u32,
            Encoding::Utf32 => 1,
        }
    }
}

/// Byte offset of the start of every line.
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\n' => starts.push(i + 1),
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                starts.push(i + 2);
                i += 1;
            }
            b'\r' => starts.push(i + 1),
            _ => {}
        }
        i += 1;
    }
    starts
}

/// End of the line content starting at `start`: the offset of its terminator,
/// or the end of the text.
fn content_end(text: &str, start: usize) -> usize {
    text.get(start..)
        .and_then(|rest| rest.find(['\n', '\r']))
        .map_or(text.len(), |i| start + i)
}

/// The byte offset of `pos`, or `None` when it lies past the end of its line or
/// past the last line. A column inside a multi-unit character (the second half
/// of a surrogate pair, say) lands on that character's first byte.
pub(crate) fn to_offset(text: &str, pos: Position, enc: Encoding) -> Option<usize> {
    let start = *line_starts(text).get(pos.line as usize)?;
    let end = content_end(text, start);
    let line = text.get(start..end)?;
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        let next = units + enc.units(c);
        if pos.character < next {
            return Some(start + i);
        }
        units = next;
    }
    (units == pos.character).then_some(end)
}

/// The position of byte `offset` (clamped into the text, and back to a
/// character boundary).
pub(crate) fn to_position(text: &str, offset: usize, enc: Encoding) -> Position {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let starts = line_starts(text);
    let line = starts.partition_point(|&s| s <= offset).saturating_sub(1);
    let start = starts.get(line).copied().unwrap_or(0);
    let end = content_end(text, start).min(offset);
    let character = text
        .get(start..end)
        .map_or(0, |s| s.chars().map(|c| enc.units(c)).sum());
    Position::new(line as u32, character)
}

/// The byte offset of a `starlark` codemap position: lines split at `\n` only,
/// columns counted in characters. Clamped to the end of that line.
pub(crate) fn codemap_to_offset(text: &str, line: u32, column: u32) -> usize {
    let mut start = 0;
    for _ in 0..line {
        match text.get(start..).and_then(|rest| rest.find('\n')) {
            Some(i) => start += i + 1,
            None => return text.len(),
        }
    }
    let end = text
        .get(start..)
        .and_then(|rest| rest.find('\n'))
        .map_or(text.len(), |i| start + i);
    let line_text = text.get(start..end).unwrap_or("");
    line_text
        .char_indices()
        .nth(column as usize)
        .map_or(end, |(i, _)| start + i)
}

/// `(line, byte column)` of byte `offset`, with lines split at `\n` only: the
/// coordinates `starlark`'s codemap and the stock analysis index by.
pub(crate) fn newline_position(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let before = text.get(..offset).unwrap_or("");
    let line = before.matches('\n').count() as u32;
    let col = before.rfind('\n').map_or(offset, |i| offset - i - 1) as u32;
    Position::new(line, col)
}

/// A codemap position (lines at `\n`, character columns) re-expressed in the
/// negotiated encoding, against the same text.
pub(crate) fn from_codemap(text: &str, pos: Position, enc: Encoding) -> Position {
    to_position(text, codemap_to_offset(text, pos.line, pos.character), enc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    #[test]
    fn negotiation_takes_the_first_supported_and_fails_closed_to_utf16() {
        let k = |s: &str| PositionEncodingKind::from(s.to_string());
        assert_eq!(Encoding::negotiate(None), Encoding::Utf16);
        assert_eq!(Encoding::negotiate(Some(&[])), Encoding::Utf16);
        assert_eq!(
            Encoding::negotiate(Some(&[k("utf-8"), k("utf-16")])),
            Encoding::Utf8
        );
        assert_eq!(Encoding::negotiate(Some(&[k("utf-32")])), Encoding::Utf32);
        assert_eq!(Encoding::negotiate(Some(&[k("latin-1")])), Encoding::Utf16);
    }

    #[test]
    fn utf16_columns_count_surrogate_pairs_as_two() {
        let text = "a😀b\n";
        assert_eq!(to_offset(text, p(0, 1), Encoding::Utf16), Some(1));
        // Inside the pair: lands on the emoji's first byte.
        assert_eq!(to_offset(text, p(0, 2), Encoding::Utf16), Some(1));
        assert_eq!(to_offset(text, p(0, 3), Encoding::Utf16), Some(5));
        assert_eq!(to_offset(text, p(0, 2), Encoding::Utf32), Some(5));
        assert_eq!(to_offset(text, p(0, 5), Encoding::Utf8), Some(5));
        assert_eq!(to_position(text, 5, Encoding::Utf16), p(0, 3));
        assert_eq!(to_position(text, 5, Encoding::Utf32), p(0, 2));
    }

    #[test]
    fn past_the_end_has_no_offset() {
        let text = "ab\ncd";
        assert_eq!(to_offset(text, p(0, 2), Encoding::Utf16), Some(2));
        assert_eq!(to_offset(text, p(0, 3), Encoding::Utf16), None);
        assert_eq!(to_offset(text, p(1, 2), Encoding::Utf16), Some(5));
        assert_eq!(to_offset(text, p(2, 0), Encoding::Utf16), None);
        assert_eq!(to_offset("", p(0, 0), Encoding::Utf16), Some(0));
    }

    #[test]
    fn crlf_and_lone_cr_end_lines() {
        let text = "ab\r\ncd\ref";
        assert_eq!(line_starts(text), vec![0, 4, 7]);
        assert_eq!(to_offset(text, p(1, 1), Encoding::Utf16), Some(5));
        assert_eq!(to_offset(text, p(0, 3), Encoding::Utf16), None);
        assert_eq!(to_offset(text, p(2, 2), Encoding::Utf16), Some(9));
        assert_eq!(to_position(text, 8, Encoding::Utf16), p(2, 1));
        // The `\r` of a CRLF belongs to no column.
        assert_eq!(to_position(text, 3, Encoding::Utf16), p(0, 2));
    }

    #[test]
    fn codemap_columns_are_characters_on_newline_lines() {
        let text = "é = 1\nx😀y\n";
        assert_eq!(codemap_to_offset(text, 1, 2), 12);
        assert_eq!(from_codemap(text, p(1, 2), Encoding::Utf16), p(1, 3));
        assert_eq!(codemap_to_offset(text, 9, 0), text.len());
    }
}
