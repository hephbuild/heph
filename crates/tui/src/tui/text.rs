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
//! A failure box's log tail is the exception (error-reporting spec D6.1b,
//! which supersedes I13's strip there): [`escape_log_tail`] shows every
//! control byte escaped (`\x1b`, `\r`) instead of removing it, so nothing
//! the target printed is hidden and nothing acts on the terminal; and, in
//! colour on a terminal, it keeps the target's own SGR colours, each line
//! reset at its end. The dashboard's Log tab and LAST LINE keep stripping
//! everything.
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
/// and newline), DEL, or C1; or a bidi embedding, override or isolate,
/// which would make the terminal draw the text after it reordered (U+202E
/// shows `exe.txt` for `txt.exe`).
fn is_control(c: char) -> bool {
    matches!(
        c,
        '\0'..='\x08' | '\x0b'..='\x1f' | '\x7f'..='\u{9f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Whether a C1 character opens a string (DCS, SOS, OSC, PM, APC) that runs
/// to a string terminator.
fn opens_string(c: char) -> bool {
    matches!(c, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}')
}

/// The longest SGR parameter run [`escape_log_tail`] keeps: a
/// truecolor foreground and background with attributes fit with room to
/// spare; a longer run is not a colour a target meant, and goes whole.
const SGR_PARAMS_MAX: usize = 64;

/// The reset a line that kept an SGR ends with, so its colour never reaches
/// what is drawn after it.
pub const SGR_RESET: &str = "\x1b[0m";

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
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
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
            '\u{9b}' => {}
            c if opens_string(c) => {
                skip_string(&mut chars);
                continue;
            }
            _ => continue,
        }
        // A CSI: parameters and intermediates, then one final byte.
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

/// The length in bytes of the SGR `s` starts with, `ESC [ <params> m` with
/// params digits and `;` only, at most [`SGR_PARAMS_MAX`] of them; `None`
/// when `s` does not start with one. An SGR is ASCII, so its length in bytes
/// is its length in chars.
fn sgr_len(s: &str) -> Option<usize> {
    let rest = s.strip_prefix("\x1b[")?;
    let params = rest
        .bytes()
        .take(SGR_PARAMS_MAX + 1)
        .take_while(|b| b.is_ascii_digit() || *b == b';')
        .count();
    (params <= SGR_PARAMS_MAX && rest.as_bytes().get(params) == Some(&b'm')).then_some(params + 3)
}

/// `c`, a control character, as text that names it: `\r`, `\x1b`, `\x7f`,
/// and a C1 or bidi control as `\u{9b}`.
fn push_escaped(c: char, out: &mut String) {
    match c {
        '\r' => out.push_str("\\r"),
        '\0'..='\x7f' => out.push_str(&format!("\\x{:02x}", u32::from(c))),
        _ => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
    }
}

/// A failure box's log tail as it is printed (D6.1b): nothing in it acts on
/// the terminal, and nothing is hidden.
///
/// - Tab and newline stay. A CR right before a newline is part of the line
///   break (CRLF) and goes with it.
/// - With `keep_sgr` (the box goes to a terminal, in colour), an SGR
///   (`ESC [ <params> m`, params digits and `;` only, at most
///   [`SGR_PARAMS_MAX`]) is kept, and a line that kept one ends with
///   [`SGR_RESET`]. Without it, an SGR is escaped like any other sequence.
/// - Every other control character is shown escaped, never removed: `\r`,
///   `\x1b` (the rest of its sequence is printable and stays as text), and a
///   C1 or bidi control as `\u{9b}`. A progress line reads `10%\r20%`, and
///   an `ESC [?1049h` reads `\x1b[?1049h` instead of entering the alternate
///   screen.
///
/// The text is already UTF-8 (a log is decoded lossily, invalid bytes
/// U+FFFD). Cut a line of the result with [`fit_styled`], which never counts
/// or splits a kept SGR.
pub fn escape_log_tail(text: &str, keep_sgr: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let more = lines.peek().is_some();
        let line = if more {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };
        let mut kept = false;
        let mut chars = line.char_indices();
        while let Some((i, c)) = chars.next() {
            if !is_control(c) {
                out.push(c);
                continue;
            }
            if keep_sgr && let Some(sgr) = line.get(i..).and_then(|s| s.get(..sgr_len(s)?)) {
                out.push_str(sgr);
                kept = true;
                // Past the rest of the SGR, one char per byte.
                chars.nth(sgr.len() - 2);
                continue;
            }
            push_escaped(c, &mut out);
        }
        if kept {
            out.push_str(SGR_RESET);
        }
        if more {
            out.push('\n');
        }
    }
    out
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

/// A line from [`escape_log_tail`] split into its pieces: `true` for
/// an SGR, `false` for text. Its only ESC opens a kept SGR, which runs to
/// its `m`.
fn styled_pieces(line: &str) -> impl Iterator<Item = (bool, &str)> {
    let mut rest = line;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (piece, sgr) = if rest.starts_with('\x1b') {
            let end = rest.find('m').map_or(rest.len(), |i| i + 1);
            (rest.split_at(end), true)
        } else {
            let end = rest.find('\x1b').unwrap_or(rest.len());
            (rest.split_at(end), false)
        };
        rest = piece.1;
        Some((sgr, piece.0))
    })
}

/// A line from [`escape_log_tail`] laid out as [`expand_tabs`] then
/// [`cut_right`] lay out its text: tabs expanded and the text cut to `max`
/// columns, measured without the SGRs. Every SGR stays, a cut's ellipsis
/// sitting where the text stopped, so the line still ends with its reset.
pub fn fit_styled(line: &str, max: usize) -> String {
    let mut expanded = String::with_capacity(line.len());
    let mut visible = String::with_capacity(line.len());
    let mut col = 0;
    for (sgr, piece) in styled_pieces(line) {
        if sgr {
            expanded.push_str(piece);
            continue;
        }
        for c in piece.chars() {
            if c == '\t' {
                let pad = TAB_STOP - col % TAB_STOP;
                expanded.extend(std::iter::repeat_n(' ', pad));
                visible.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            } else {
                expanded.push(c);
                visible.push(c);
                col += c.width().unwrap_or(0);
            }
        }
    }
    if width(&visible) <= max {
        return expanded;
    }
    // Bytes of visible text to keep: `take_width` decides, as `cut_right`.
    let mut left = if max == 0 {
        0
    } else {
        take_width(&visible, max - 1).len()
    };
    let mut cut = false;
    let mut out = String::with_capacity(expanded.len());
    for (sgr, piece) in styled_pieces(&expanded) {
        if sgr {
            out.push_str(piece);
        } else if !cut {
            if piece.len() <= left {
                out.push_str(piece);
                left -= piece.len();
            } else {
                out.push_str(piece.get(..left).unwrap_or(""));
                if max > 0 {
                    out.push_str(ELLIPSIS);
                }
                cut = true;
                left = 0;
            }
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

/// The longest prefix of `s` at most `max` columns wide, as [`width`]
/// measures it. Never cuts inside a character; combining marks stay with the
/// character they follow.
pub fn take_width(s: &str, max: usize) -> &str {
    let mut used = 0;
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            end = i;
            break;
        }
        used += w;
    }
    let mut prefix = s.get(..end).unwrap_or(s);
    // Per character, an emoji presentation selector adds nothing, though
    // it widens the emoji before it (`❤️` is 2 columns, `❤` and U+FE0F sum
    // to 1): drop characters until the prefix fits as a whole.
    while width(prefix) > max {
        let Some(last) = prefix.chars().next_back() else {
            break;
        };
        prefix = prefix.get(..prefix.len() - last.len_utf8()).unwrap_or("");
    }
    prefix
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
    let mut tail = s.get(start..).unwrap_or("");
    // As in `take_width`: what fits per character may not as a whole.
    while width(tail) > budget {
        let Some(first) = tail.chars().next() else {
            break;
        };
        tail = tail.get(first.len_utf8()..).unwrap_or("");
    }
    // A tail that begins on a combining mark lost its base character.
    let tail = tail.trim_start_matches(|c: char| c.width() == Some(0));
    Cow::Owned(format!("{ELLIPSIS}{tail}"))
}

/// What a screen shows in place of heph's home (I9).
pub const HOME_TOKEN: &str = "<heph home>";

/// Paths no screen shows (I9): heph's home, and with it every sandbox, which
/// lives under it. Error text is how they reach a screen, so it passes
/// through [`Redact::apply`] before it is drawn.
#[derive(Debug, Clone, Default)]
pub struct Redact {
    /// Longest first, so a root that extends another is matched whole.
    roots: Vec<String>,
}

impl Redact {
    /// heph's home as given and as the filesystem resolves it (`/var` and
    /// `/private/var` on macOS): an error can name either. Reads the
    /// filesystem once, so it is built before the first frame, never in one.
    pub fn heph_home(home: &std::path::Path) -> Self {
        let mut roots = vec![home.to_path_buf()];
        if let Ok(canonical) = std::fs::canonicalize(home)
            && canonical != home
        {
            roots.push(canonical);
        }
        Self::of(roots)
    }

    /// Redact each of `roots`. `/` and empty roots are dropped: they would
    /// redact every path.
    pub fn of(roots: impl IntoIterator<Item = std::path::PathBuf>) -> Self {
        let mut roots: Vec<String> = roots
            .into_iter()
            .filter_map(|p| {
                let s = p.to_string_lossy().trim_end_matches('/').to_owned();
                (!s.is_empty()).then_some(s)
            })
            .collect();
        roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
        roots.dedup();
        Self { roots }
    }

    /// `s` with every whole-path occurrence of a root replaced by
    /// [`HOME_TOKEN`]. `/h/.heph` in `/h/.heph2` or `/x/h/.heph` names
    /// another directory and is kept. Borrows when nothing matches.
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(s);
        for root in &self.roots {
            if out.contains(root.as_str()) {
                out = Cow::Owned(replace_path(&out, root));
            }
        }
        out
    }
}

/// The workspace root, as heph's own messages name paths under it: relative
/// to it (`/ws/a/BUILD` reads `a/BUILD`, `/ws` reads `.`). Messages only —
/// never a log tail or a target's output, which say what the tool printed
/// (D2).
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRel {
    /// Longest first, so a root that extends another is matched whole.
    roots: Vec<String>,
}

impl WorkspaceRel {
    /// The root as given and as the filesystem resolves it (`/var` and
    /// `/private/var` on macOS): an error can name either. Reads the
    /// filesystem once, so it is built before the render, never in one.
    pub fn workspace(root: &std::path::Path) -> Self {
        let mut roots = vec![root.to_path_buf()];
        if let Ok(canonical) = std::fs::canonicalize(root)
            && canonical != root
        {
            roots.push(canonical);
        }
        Self::of(roots)
    }

    /// Rewrite paths under each of `roots`. `/` and empty roots are dropped:
    /// every absolute path would become relative to them.
    pub fn of(roots: impl IntoIterator<Item = std::path::PathBuf>) -> Self {
        let Redact { roots } = Redact::of(roots);
        Self { roots }
    }

    /// `s` with each path at or under a root, matched at a component
    /// boundary, made relative to it: `/ws/a/` reads `a/` (a trailing slash
    /// is kept as written), `/ws` reads `.`, and `/ws-other/a` or `/x/ws/a`
    /// name other directories and are kept. Borrows when nothing matches.
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(s);
        for root in &self.roots {
            if out.contains(root.as_str()) {
                out = Cow::Owned(relativize(&out, root));
            }
        }
        out
    }
}

fn relativize(s: &str, root: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(root) {
        let (before, from) = rest.split_at(i);
        let after = from.get(root.len()..).unwrap_or("");
        out.push_str(before);
        let starts = out.chars().next_back().is_none_or(|c| !is_path_char(c));
        let mut tail = after.chars();
        match tail.next() {
            // `/ws/a`: the path below the root, without its leading `/`;
            // `/ws/` alone is the root's own directory, `./`.
            Some('/') if starts => {
                let below = tail.as_str();
                if below.chars().next().is_none_or(|c| !is_path_char(c)) {
                    out.push_str("./");
                }
                rest = below;
            }
            // `/ws` itself, at the end or before a `:` or a space.
            next if starts && next.is_none_or(|c| !is_path_char(c)) => {
                out.push('.');
                rest = after;
            }
            // `/ws-other`, `/x/ws`: another directory.
            _ => {
                out.push_str(root);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// heph's own message — an error's text, a hint, a value — as a screen draws
/// it: heph's home redacted (I9), paths under the workspace made relative
/// ([`WorkspaceRel`]), and every control byte but newline and tab shown
/// escaped (`\x1b`), so a message never moves the cursor (D2). Never for a
/// log tail, which keeps its paths and has its controls removed
/// ([`sanitize_log`]). Borrows when there is nothing to change.
pub fn message<'a>(s: &'a str, redact: &Redact, rel: &WorkspaceRel) -> Cow<'a, str> {
    let s = redact.apply(s);
    let s = match rel.apply(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    };
    match hcore::diagnostic::escape_control(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    }
}

/// A character that continues a path component: a root followed or
/// preceded by one is part of a longer name.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '@' | '/')
}

fn replace_path(s: &str, root: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(root) {
        let (before, from) = rest.split_at(i);
        let after = from.get(root.len()..).unwrap_or("");
        out.push_str(before);
        // `before` is checked against what was already written, so a match
        // right after a replaced one sees the token's `>`, not a path char.
        let starts = out.chars().next_back().is_none_or(|c| !is_path_char(c));
        let ends = after
            .chars()
            .next()
            .is_none_or(|c| c == '/' || !is_path_char(c));
        out.push_str(if starts && ends { HOME_TOKEN } else { root });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether `haystack` holds `needle`, ASCII letters compared without case
/// and every other byte as-is: what lower-casing both with
/// `to_ascii_lowercase` and calling `contains` decides, without the two
/// `String`s. An empty needle is in everything.
///
/// The `/` filter tries it on every address to count its matches, and the
/// log search on every held line.
pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    // `windows` yields nothing when the needle is longer than the haystack,
    // and the empty needle is handled above, so the width is never zero.
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace `/ws` of the spec's `relpath_<row>` table.
    fn ws() -> WorkspaceRel {
        WorkspaceRel::of(["/ws".into()])
    }

    fn rel(s: &str) -> String {
        message(s, &Redact::default(), &ws()).into_owned()
    }

    #[test]
    fn relpath_under_root() {
        assert_eq!(rel("/ws/a/BUILD"), "a/BUILD");
        assert_eq!(
            rel("evaluating /ws/a/BUILD:4:5: \"/ws/b\""),
            "evaluating a/BUILD:4:5: \"b\""
        );
    }

    #[test]
    fn relpath_sibling_prefix_unchanged() {
        assert_eq!(rel("/ws-other/a/BUILD"), "/ws-other/a/BUILD");
        assert_eq!(rel("/x/ws/a"), "/x/ws/a");
        assert_eq!(rel("/wsx"), "/wsx");
    }

    #[test]
    fn relpath_root_itself() {
        assert_eq!(rel("/ws"), ".");
        assert_eq!(rel("in /ws: denied"), "in .: denied");
        assert_eq!(rel("/ws/"), "./");
    }

    #[test]
    fn relpath_trailing_slash_kept() {
        assert_eq!(rel("/ws/a/"), "a/");
    }

    /// macOS: the workspace is given as `/var/w`, the error names the path
    /// the filesystem resolved, `/private/var/w/a`. [`WorkspaceRel::workspace`]
    /// holds both; the shorter never matches inside the longer.
    #[test]
    fn relpath_canonical_root() {
        let rel = WorkspaceRel::of(["/var/w".into(), "/private/var/w".into()]);
        assert_eq!(rel.apply("/private/var/w/a"), "a");
        assert_eq!(rel.apply("/var/w/a"), "a");
        assert_eq!(rel.apply("/private/var/wx"), "/private/var/wx");
    }

    /// And the real thing: a root reached through a symlink is matched by
    /// both spellings, as `workspace` reads the filesystem for the second.
    #[test]
    fn relpath_canonical_root_from_the_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let rel = WorkspaceRel::workspace(&link);
        let canonical = std::fs::canonicalize(&link).expect("canonicalize");
        let named = format!("{}/a/BUILD", canonical.display());
        assert_eq!(rel.apply(&named), "a/BUILD");
        assert_eq!(rel.apply(&format!("{}/a", link.display())), "a");
    }

    #[test]
    fn relpath_outside_unchanged() {
        assert_eq!(rel("/etc/x"), "/etc/x");
        assert!(matches!(ws().apply("/etc/x"), Cow::Borrowed(_)));
        // `/` is no workspace: it would make every path relative.
        assert_eq!(WorkspaceRel::of(["/".into()]).apply("/etc/x"), "/etc/x");
    }

    /// Messages only: a log tail goes through [`sanitize_log`], which keeps
    /// the tool's own paths as it printed them.
    #[test]
    fn relpath_log_tail_unchanged() {
        assert_eq!(sanitize_log(b"/ws/a/BUILD\n"), "/ws/a/BUILD\n");
        assert_eq!(rel("/ws/a/BUILD"), "a/BUILD");
    }

    /// heph's home is redacted before the workspace is: `/ws/.heph` is the
    /// home, not `.heph`.
    #[test]
    fn message_redacts_home_before_relativizing() {
        let home = Redact::of(["/ws/.heph".into()]);
        assert_eq!(
            message("/ws/.heph/sandbox/x and /ws/a", &home, &ws()),
            format!("{HOME_TOKEN}/sandbox/x and a")
        );
    }

    /// A control byte in a message is shown, not obeyed: a clear-screen or a
    /// cursor move arrives as text. Newline and tab stay.
    #[test]
    fn message_control_bytes_escaped() {
        let shown = message(
            "a\x1b[2Jb\rc\u{9b}d\n\te",
            &Redact::default(),
            &WorkspaceRel::default(),
        );
        assert_eq!(shown, "a\\x1b[2Jb\\x0dc\\u{9b}d\n\te");
        assert!(matches!(
            message("plain", &Redact::default(), &WorkspaceRel::default()),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn contains_folds_ascii_case_only() {
        let contains = contains_ignore_ascii_case;
        assert!(contains("//WEB:Server", "web"));
        assert!(contains("//web:server", "WEB:SERVER"));
        assert!(contains("//web:server", "//web:server"));
        assert!(!contains("//web:server", "api"));
        // At either end, and overlapping a near miss.
        assert!(contains("abc", "AB") && contains("abc", "bC"));
        assert!(contains("aab", "ab"));
        // A needle longer than the haystack can never match.
        assert!(!contains("//a:b", "//a:b:c"));
        // The empty needle matches everything, the empty haystack included.
        assert!(contains("//a:b", "") && contains("", ""));
        assert!(!contains("", "a"));
        // Non-ASCII bytes compare as-is: exactly what ASCII lower-casing did.
        assert!(contains("//pkg/ünï:t", "ünï"));
        assert!(!contains("//pkg/ünï:t", "ÜNÏ"));
        for (h, n) in [
            ("Größe X", "SSE"),
            ("ÀB", "àb"),
            ("x\u{1F600}Y", "\u{1F600}y"),
        ] {
            assert_eq!(
                contains(h, n),
                h.to_ascii_lowercase().contains(&n.to_ascii_lowercase()),
                "{h:?} {n:?}"
            );
        }
    }

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
            // Every control string, by escape and by C1 introducer.
            (b"\x1bPq#0;2;0;0;0\x1b\\dcs", "dcs"),
            (b"\x1bXsos\x1b\\a", "a"),
            (b"\x1b^pm\x07b", "b"),
            (b"\x1b_apc\x1b\\c", "c"),
            (b"\xc2\x90dcs\xc2\x9cz", "z"),
            (b"\xc2\x98sos\xc2\x9cy", "y"),
            // Bidi overrides, embeddings and isolates.
            ("a\u{202e}fdp.exe".as_bytes(), "afdp.exe"),
            ("\u{2066}x\u{2069} \u{202a}y\u{202c}".as_bytes(), "x y"),
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

    /// D6.1b, one row each: on a terminal in colour (`tty`) an SGR is kept
    /// and its line reset; everywhere else, and for every other control
    /// byte, the byte is shown escaped, never removed; tab and newline stay.
    /// Then the adversarial rows: SGR look-alikes that are not one, a
    /// sequence split across lines, an over-long parameter run, C1 and bidi
    /// controls.
    #[test]
    fn log_tail_controls_shown_escaped() {
        let long = format!("\x1b[{}mx", "1;".repeat(SGR_PARAMS_MAX));
        let at_max = format!("\x1b[{}mx", "1".repeat(SGR_PARAMS_MAX));
        let cases: &[(&str, &str, bool, &str)] = &[
            (
                "sgr kept on a tty",
                "\x1b[31mred\x1b[0m",
                true,
                "\x1b[31mred\x1b[0m\x1b[0m",
            ),
            (
                "sgr escaped when not",
                "\x1b[31mred\x1b[0m",
                false,
                "\\x1b[31mred\\x1b[0m",
            ),
            ("cr escaped", "10%\r20%\r30%\n", true, "10%\\r20%\\r30%\n"),
            (
                "alternate screen escaped",
                "x\x1b[?1049hy",
                true,
                "x\\x1b[?1049hy",
            ),
            ("cursor hide escaped", "\x1b[?25lz", true, "\\x1b[?25lz"),
            ("tab and newline kept", "a\tb\nc\n", true, "a\tb\nc\n"),
            ("crlf is a line break", "a\r\nb\r\n", true, "a\nb\n"),
            ("clear escaped", "\x1b[2Jc", true, "\\x1b[2Jc"),
            (
                "osc escaped",
                "\x1b]52;c;aGk=\x07z",
                true,
                "\\x1b]52;c;aGk=\\x07z",
            ),
            ("nul and del", "a\0b\x7f", true, "a\\x00b\\x7f"),
            ("c1 csi", "\u{9b}31mx", true, "\\u{9b}31mx"),
            ("bidi override", "a\u{202e}b", true, "a\\u{202e}b"),
            // Not an SGR: a private marker, a colon, an intermediate byte.
            ("private m", "\x1b[?31mx", true, "\\x1b[?31mx"),
            ("colon m", "\x1b[38:2:1mx", true, "\\x1b[38:2:1mx"),
            ("intermediate m", "\x1b[31 mx", true, "\\x1b[31 mx"),
            ("split across lines", "a\x1b[3\n1mb", true, "a\\x1b[3\n1mb"),
            ("unterminated", "t \x1b[31", true, "t \\x1b[31"),
            ("over long", &long, true, &long.replace('\x1b', "\\x1b")),
            ("at the cap", &at_max, true, &format!("{at_max}{SGR_RESET}")),
            ("bare sgr", "\x1b[mx", true, "\x1b[mx\x1b[0m"),
            (
                "no reset without an sgr",
                "\x1b[31mr\nplain\n",
                true,
                "\x1b[31mr\x1b[0m\nplain\n",
            ),
            (
                "invalid utf8 already lossy",
                "a\u{fffd}b",
                true,
                "a\u{fffd}b",
            ),
        ];
        for (name, input, tty, want) in cases {
            let got = escape_log_tail(input, *tty);
            assert_eq!(got, *want, "{name}");
            // The only control left is an ESC opening a kept SGR.
            let plain = sanitize_log(got.as_bytes());
            assert!(!has_control(&plain), "{name}: {plain:?}");
            if !tty {
                assert!(!got.contains('\x1b'), "{name}: {got:?}");
            }
        }
    }

    /// A styled line is laid out by its text alone: tabs and the cut ignore
    /// the SGRs, which all stay, and the reset still ends the line.
    #[test]
    fn fit_styled_measures_text_and_keeps_every_sgr() {
        let red = "\x1b[31m";
        let line = format!("{red}abcdef{SGR_RESET}");
        assert_eq!(fit_styled(&line, 6), line);
        assert_eq!(fit_styled(&line, 4), format!("{red}abc…{SGR_RESET}"));
        assert_eq!(fit_styled(&line, 1), format!("{red}…{SGR_RESET}"));
        assert_eq!(fit_styled(&line, 0), format!("{red}{SGR_RESET}"));
        // A cut inside the second run: the first run's SGR, its text, then
        // the cut, and the SGRs after it.
        let two = format!("{red}ab\x1b[32mcd\x1b[1mef{SGR_RESET}");
        assert_eq!(
            fit_styled(&two, 4),
            format!("{red}ab\x1b[32mc…\x1b[1m{SGR_RESET}")
        );
        // Tabs expand by visible column.
        assert_eq!(
            fit_styled(&format!("{red}a\tb{SGR_RESET}"), 20),
            format!("{red}a       b{SGR_RESET}")
        );
        // Wide characters are measured by display width.
        assert_eq!(
            fit_styled(&format!("{red}宽字宽{SGR_RESET}"), 4),
            format!("{red}宽…{SGR_RESET}")
        );
        // Unstyled, it is cut_right of expand_tabs.
        for (s, max) in [("a\tbcdefghijkl", 9), ("宽字宽字", 5), ("short", 10)] {
            assert_eq!(fit_styled(s, max), cut_right(&expand_tabs(s), max));
        }
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
        // An emoji with its presentation selector is 2 columns as a whole,
        // 1 summed per character: a cut measures it whole.
        assert_eq!(width("⚠\u{fe0f}"), 2);
        assert_eq!(take_width("⚠\u{fe0f} warn", 1), "⚠");
        assert_eq!(take_width("⚠\u{fe0f} warn", 2), "⚠\u{fe0f}");
        assert_eq!(cut_right("⚠\u{fe0f} warning", 2), "⚠…");
        assert_eq!(cut_left("warning ⚠\u{fe0f}", 2), "…");
        for s in [
            "//宽字:目标",
            "//e\u{301}\u{301}x:y",
            "plain//addr",
            "⚠\u{fe0f} disk ⚠\u{fe0f} low",
            "❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}",
            "a👩\u{200d}💻b",
        ] {
            for max in 0..12 {
                assert!(width(&cut_left(s, max)) <= max, "{s} at {max}");
                assert!(width(&cut_right(s, max)) <= max, "{s} at {max}");
                assert!(width(take_width(s, max)) <= max, "{s} at {max}");
            }
        }
    }

    /// I9: heph's home, and every sandbox under it, is replaced as a whole
    /// path; a near-miss directory is another path and stays.
    #[test]
    fn redact_replaces_whole_home_paths_only() {
        let r = Redact::of([
            std::path::PathBuf::from("/h/.heph/"),
            std::path::PathBuf::from("/private/h/.heph"),
            std::path::PathBuf::from("/"),
        ]);
        let cases = [
            (
                "open /h/.heph/sandbox/ab/run.sh: denied",
                "open <heph home>/sandbox/ab/run.sh: denied",
            ),
            ("/h/.heph", "<heph home>"),
            ("'/h/.heph/x'", "'<heph home>/x'"),
            ("/private/h/.heph/x", "<heph home>/x"),
            ("/h/.heph2/x", "/h/.heph2/x"),
            ("/x/h/.heph/y", "/x/h/.heph/y"),
            ("a /h/.heph b /h/.heph/c", "a <heph home> b <heph home>/c"),
            ("/h/.hephé/x", "/h/.hephé/x"),
            ("no path", "no path"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(r.apply(input), want, "{input:?}");
        }
        assert!(matches!(r.apply("no path"), Cow::Borrowed(_)));
        assert_eq!(Redact::default().apply("/h/.heph"), "/h/.heph");
    }
}
