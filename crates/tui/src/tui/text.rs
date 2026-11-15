//! Text that came from outside heph — log bytes, addresses, error messages —
//! made safe to draw and measured the way a terminal lays it out.
//!
//! I13: log bytes never reach the terminal as commands. Every screen draws
//! log text through [`sanitize_log`] (or [`clean_inline`] for a one-line
//! field), which removes every C0 and C1 control byte apart from tab and
//! newline, whole escape sequences included, so an `ESC[?1049l` in a target's
//! output cannot leave the alternate screen and an OSC 52 cannot write the
//! clipboard. Only `heph log` writes a log as it is, to its own stdout.
//!
//! Widths are display widths (`unicode-width`): a CJK character takes two
//! columns, a combining mark none, and a cut never lands inside a character
//! or between a character and its combining marks.

use std::borrow::Cow;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Columns between tab stops when [`expand_tabs`] lays a line out.
pub const TAB_STOP: usize = 8;

/// The ellipsis a cut puts where text was removed.
pub const ELLIPSIS: &str = "…";

/// Whether `c` is a control character a terminal could act on: C0 (but tab
/// and newline), DEL, or C1.
fn is_control(c: char) -> bool {
    matches!(c, '\0'..='\x08' | '\x0b'..='\x1f' | '\x7f'..='\u{9f}')
}

/// Whether a C1 character opens a string (DCS, SOS, OSC, PM, APC) that runs
/// to a string terminator.
fn opens_string(c: char) -> bool {
    matches!(c, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}')
}

/// Strip control characters from `s`, keeping tab and newline. An escape
/// sequence goes whole: CSI (`ESC [` or U+009B) up to its final byte, and a
/// control string (OSC, DCS, SOS, PM, APC) up to BEL or ST. A sequence that
/// breaks off early loses only its introducer, and its remaining text stays.
fn strip_controls(s: &str, out: &mut String) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if !is_control(c) {
            out.push(c);
            continue;
        }
        let csi = match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    true
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    skip_string(&mut chars);
                    continue;
                }
                // Any other escape: the introducer and the one character it
                // selects (`ESC c` resets the terminal, `ESC 7` saves the
                // cursor). Only a printable one is part of the sequence.
                Some('\x20'..='\x7e') => {
                    chars.next();
                    continue;
                }
                _ => continue,
            },
            '\u{9b}' => true,
            c if opens_string(c) => {
                skip_string(&mut chars);
                continue;
            }
            _ => continue,
        };
        if csi {
            // Parameters and intermediates, then one final byte.
            while let Some(&n) = chars.peek() {
                match n {
                    '\x20'..='\x3f' => {
                        chars.next();
                    }
                    '\x40'..='\x7e' => {
                        chars.next();
                        break;
                    }
                    _ => break,
                }
            }
        }
    }
}

/// Skip a control string's body up to and including its terminator: BEL,
/// `ESC \`, or U+009C. An unterminated string runs to the end of the text.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        match c {
            '\x07' | '\u{9c}' => return,
            '\x1b' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                return;
            }
            _ => {}
        }
    }
}

/// Log bytes as text a screen can draw (I13).
///
/// - Invalid UTF-8 reads as U+FFFD, a byte at a time.
/// - CRLF is a newline. A bare CR returns to the start of the line, so the
///   line shows what was written last: `10%\r20%\r30%` reads `30%`.
/// - Every other C0 and C1 control is removed, escape sequences whole. Tab
///   and newline stay; [`expand_tabs`] lays tabs out for a cell grid.
pub fn sanitize_log(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let shown = line.rsplit('\r').next().unwrap_or(line);
        strip_controls(shown, &mut out);
        if lines.peek().is_some() {
            out.push('\n');
        }
    }
    out
}

/// A one-line field from outside heph (an address, the first line of an
/// error) as text a single row can draw: controls removed as
/// [`sanitize_log`] removes them, and a tab or newline read as one space.
/// Borrows when there is nothing to change, which is every frame of a
/// well-formed build.
pub fn clean_inline(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| is_control(c) || c == '\t' || c == '\n') {
        return Cow::Borrowed(s);
    }
    let mut stripped = String::with_capacity(s.len());
    strip_controls(s, &mut stripped);
    Cow::Owned(stripped.replace(['\t', '\n'], " "))
}

/// Tabs replaced by spaces up to the next [`TAB_STOP`], measured in display
/// columns from the start of `line`.
pub fn expand_tabs(line: &str) -> Cow<'_, str> {
    if !line.contains('\t') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len() + TAB_STOP);
    let mut col = 0;
    for c in line.chars() {
        if c == '\t' {
            let pad = TAB_STOP - col % TAB_STOP;
            out.extend(std::iter::repeat_n(' ', pad));
            col += pad;
        } else {
            out.push(c);
            col += c.width().unwrap_or(0);
        }
    }
    Cow::Owned(out)
}

/// `s`'s width in terminal columns.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The longest prefix of `s` at most `max` columns wide. Never cuts inside a
/// character; combining marks stay with the character they follow.
pub fn take_width(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            return s.get(..i).unwrap_or(s);
        }
        used += w;
    }
    s
}

/// `s` cut to `max` columns from the right: kept whole when it fits,
/// otherwise its head and an ellipsis. Empty when `max` is 0.
pub fn cut_right(s: &str, max: usize) -> Cow<'_, str> {
    if width(s) <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    Cow::Owned(format!("{}{ELLIPSIS}", take_width(s, max - 1)))
}

/// `s` cut to `max` columns from the left: kept whole when it fits,
/// otherwise an ellipsis and its tail — the end of an address is the part
/// that names it. The tail never starts with a combining mark cut from its
/// character. Empty when `max` is 0.
pub fn cut_left(s: &str, max: usize) -> Cow<'_, str> {
    if width(s) <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let budget = max - 1;
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start = i;
    }
    let tail = s.get(start..).unwrap_or("");
    // A tail that begins on a combining mark lost its base character.
    let tail = tail.trim_start_matches(|c: char| c.width() == Some(0));
    Cow::Owned(format!("{ELLIPSIS}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether any character a terminal would act on survived.
    fn has_control(s: &str) -> bool {
        s.chars().any(is_control)
    }

    /// The adversarial table's log-text rows, reduced to what the sanitizer
    /// alone decides: no control byte survives, CR keeps the last segment,
    /// invalid bytes are U+FFFD, tabs and newlines stay.
    #[test]
    fn control_bytes_never_reach_terminal_sanitizer() {
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"\n\n", "\n\n"),
            (b"no newline", "no newline"),
            (b"a\r\nb\r\n", "a\nb\n"),
            (b"10%\r20%\r30%", "30%"),
            (b"x\x1b[?1049ly", "xy"),
            (b"x\x1b[?1003hy", "xy"),
            (b"\x1b[2Jclear", "clear"),
            (b"\x1b[31mred\x1b[0m", "red"),
            (b"\x1b]8;;https://e.x\x1b\\link\x1b]8;;\x1b\\", "link"),
            (b"\x1b]52;c;aGk=\x07after", "after"),
            (b"lone \x9b csi", "lone \u{fffd} csi"),
            (b"c1 \xc2\x9b2J csi", "c1  csi"),
            (b"bad \xff\xfe utf8", "bad \u{fffd}\u{fffd} utf8"),
            (b"nul\0byte", "nulbyte"),
            (b"a\tb", "a\tb"),
            (b"bell\x07 del\x7f", "bell del"),
            (b"cut mid \xe6\x97", "cut mid \u{fffd}"),
            ("宽字 e\u{301}".as_bytes(), "宽字 e\u{301}"),
            (b"unterminated \x1b]0;title", "unterminated "),
            (b"trailing esc \x1b", "trailing esc "),
        ];
        for (input, want) in cases {
            let got = sanitize_log(input);
            assert_eq!(got, *want, "input {input:?}");
            assert!(!has_control(&got), "a control survived {input:?}: {got:?}");
        }
        // A megabyte with no newline comes through whole, and clean.
        let big = vec![b'x'; 1 << 20];
        assert_eq!(sanitize_log(&big).len(), 1 << 20);
    }

    #[test]
    fn clean_inline_borrows_clean_text_and_flattens_the_rest() {
        assert!(matches!(clean_inline("//a:b"), Cow::Borrowed(_)));
        assert_eq!(clean_inline("//a\x1b[2J:b\tc\nd"), "//a:b c d");
        assert_eq!(clean_inline("\u{9b}31m//x"), "//x");
    }

    #[test]
    fn tabs_expand_to_the_next_stop_by_display_width() {
        assert_eq!(expand_tabs("a\tb"), "a       b");
        assert_eq!(expand_tabs("宽\tb"), "宽      b");
        assert_eq!(expand_tabs("plain"), "plain");
    }

    #[test]
    fn cuts_measure_display_width_and_never_split_a_character() {
        assert_eq!(take_width("宽字宽", 3), "宽");
        assert_eq!(take_width("e\u{301}x", 1), "e\u{301}");
        assert_eq!(cut_right("abcdef", 4), "abc…");
        assert_eq!(cut_right("宽字宽", 4), "宽…");
        assert_eq!(cut_left("//go/cmd/heph-govet:build", 12), "…govet:build");
        assert_eq!(cut_left("//宽字宽字", 5), "…宽字");
        assert_eq!(cut_left("//ae\u{301}b", 3), "…e\u{301}b");
        // The mark alone fits, its character does not: neither is kept.
        assert_eq!(cut_left("//ae\u{301}b", 2), "…b");
        assert_eq!(cut_left("short", 10), "short");
        assert_eq!(cut_left("short", 0), "");
        for s in ["//宽字:目标", "//e\u{301}\u{301}x:y", "plain//addr"] {
            for max in 0..12 {
                assert!(width(&cut_left(s, max)) <= max, "{s} at {max}");
                assert!(width(&cut_right(s, max)) <= max, "{s} at {max}");
            }
        }
    }
}
