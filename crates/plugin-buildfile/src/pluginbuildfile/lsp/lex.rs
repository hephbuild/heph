//! The compact token array the site resolver reads, built once per `didChange`.
//!
//! `starlark_syntax`'s lexer is public and yields byte-offset tokens and lexer
//! errors as items, but on a broken document it does not always behave: an
//! unterminated string's body is re-lexed as ordinary tokens after the error,
//! an unterminated f-string yields the same error forever, and an f-string
//! whose `{` expression is left open lexes the rest of the file in expression
//! mode. The loop here owns all of that, so lexing always terminates and an
//! error never leaks past its line unless the construct itself runs on (an
//! open triple quote):
//!
//! - **High-water mark.** A token or error that starts below the end of the
//!   furthest item already kept is dropped: the re-lexed body of an
//!   unterminated string, the nested `'` error inside `"don't`.
//! - **Next-line restart.** An error that makes no progress (the same span
//!   twice), and an f-string without its end on its line, end lexing on that
//!   line: the error span is extended to the line end and a fresh lexer starts
//!   at the next line.
//! - **Hard item cap.** At `2n + 8` items lexing stops, whatever the lexer does.
//!
//! Every token keeps only its kind and span (no owned strings); lexer errors
//! are tokens too, with their message kept aside for diagnostics. `Newline`,
//! `Indent` and `Dedent` are dropped: the lexer omits them after tab and
//! indentation errors and inside brackets, so nothing may rely on them.

use starlark_syntax::codemap::CodeMap;
use starlark_syntax::lexer::{Lexer, Token};
use starlark_syntax::syntax::Dialect;

/// No link: an opener never closed, or a closer that matched nothing.
pub(crate) const NO_LINK: u32 = u32::MAX;

/// A fresh lexer per restart copies the rest of the source; past this many
/// restarts the remainder becomes one error instead, so an adversarial file
/// (an unterminated f-string on every line) stays linear.
const MAX_RESTARTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bracket {
    Round,
    Square,
    Curly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// An error span that opens with an optional `r`/`b`/`f` prefix and a
    /// quote and does not end with the matching quote: an unterminated string
    /// running from the quote to the span's end. `triple` for `"""`/`'''`.
    UnterminatedString { triple: bool },
    /// Anything else: a stray character, a bad escape, a tab, a bad dedent.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Ident,
    /// The `load` keyword.
    Load,
    /// Any other keyword (`def`, `if`, `for`, …).
    Keyword,
    /// A closed string, bytes or f-string literal, quotes and prefix included.
    Str,
    Number,
    Open(Bracket),
    Close(Bracket),
    Comma,
    Equal,
    Dot,
    Colon,
    Comment,
    /// Any other operator or punctuation.
    Op,
    /// A lexer error; its message is in [`Tokens::errors`].
    Error(ErrorKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tok {
    pub kind: Kind,
    pub start: u32,
    pub end: u32,
    /// For an opener: the closer that matched it, or the mismatched closer
    /// that popped it ([`Tok::popped`]); for a closer: its opener.
    /// [`NO_LINK`] otherwise.
    pub link: u32,
    /// An opener ended by a closer of another kind (`target( ]`).
    pub popped: bool,
}

impl Tok {
    pub(crate) fn range(&self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }

    /// Whether byte `o` lies inside this token, by the boundary rule for its
    /// kind: inside an unterminated string (or a comment, which also runs to
    /// its line end) is `(start, end]`; inside any other error or a closed
    /// string is the open interval; a zero-width span contains nothing.
    pub(crate) fn contains(&self, o: usize) -> bool {
        let (s, e) = (self.start as usize, self.end as usize);
        if s == e {
            return false;
        }
        match self.kind {
            Kind::Error(ErrorKind::UnterminatedString { .. }) | Kind::Comment => s < o && o <= e,
            _ => s < o && o < e,
        }
    }
}

/// A lexer error, for the published diagnostics.
#[derive(Clone, Debug)]
pub(crate) struct LexError {
    /// Index of its [`Kind::Error`] token.
    pub token: u32,
    pub message: String,
    /// Errors are published once per line, the first on it: a later error on
    /// the same line is a consequence (the re-opened string after a bad
    /// escape), not a root cause.
    pub publish: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Tokens {
    pub toks: Vec<Tok>,
    pub errors: Vec<LexError>,
    /// How many items the lexer yielded, for the item-cap tests.
    pub items: usize,
}

impl Tokens {
    pub(crate) fn error_message(&self, token: usize) -> Option<&str> {
        self.errors
            .iter()
            .find(|e| e.token as usize == token)
            .map(|e| e.message.as_str())
    }
}

/// The item cap for a source of `len` bytes.
pub(crate) fn item_cap(len: usize) -> usize {
    2 * len + 8
}

/// Offset of the end of the line containing `at` (its `\n`, or the end).
pub(crate) fn line_end(src: &str, at: usize) -> usize {
    src.get(at..)
        .and_then(|rest| rest.find('\n'))
        .map_or(src.len(), |i| at + i)
}

/// Offset of the start of the line containing `at`.
pub(crate) fn line_start(src: &str, at: usize) -> usize {
    src.get(..at)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |i| i + 1)
}

fn classify_error(src: &str, s: usize, e: usize) -> ErrorKind {
    let text = src.get(s..e).unwrap_or("");
    let body = text.trim_start_matches(['r', 'b', 'f', 'R', 'B', 'F']);
    if body.len() + 2 < text.len() {
        return ErrorKind::Other;
    }
    let Some(quote) = body.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return ErrorKind::Other;
    };
    let triple = body.starts_with(&quote.to_string().repeat(3));
    let opener = if triple { 3 } else { 1 };
    let closed = body.len() > opener && body.ends_with(quote);
    if closed {
        ErrorKind::Other
    } else {
        ErrorKind::UnterminatedString { triple }
    }
}

fn kind_of(tok: &Token) -> Option<Kind> {
    Some(match tok {
        Token::Newline | Token::Indent | Token::Dedent => return None,
        Token::Identifier(_) => Kind::Ident,
        Token::Load => Kind::Load,
        Token::And
        | Token::Break
        | Token::Continue
        | Token::Def
        | Token::Elif
        | Token::Else
        | Token::For
        | Token::If
        | Token::In
        | Token::Lambda
        | Token::Not
        | Token::Or
        | Token::Pass
        | Token::Return => Kind::Keyword,
        Token::String(_) | Token::Bytes(_) => Kind::Str,
        Token::Int(_) | Token::Float(_) => Kind::Number,
        Token::OpeningRound => Kind::Open(Bracket::Round),
        Token::OpeningSquare => Kind::Open(Bracket::Square),
        Token::OpeningCurly => Kind::Open(Bracket::Curly),
        Token::ClosingRound => Kind::Close(Bracket::Round),
        Token::ClosingSquare => Kind::Close(Bracket::Square),
        Token::ClosingCurly => Kind::Close(Bracket::Curly),
        Token::Comma => Kind::Comma,
        Token::Equal => Kind::Equal,
        Token::Dot => Kind::Dot,
        Token::Colon => Kind::Colon,
        Token::Comment(_) => Kind::Comment,
        _ => Kind::Op,
    })
}

struct Builder<'a> {
    src: &'a str,
    out: Tokens,
    /// End of the furthest item kept.
    hw: usize,
    /// The line start of the last error kept.
    last_error_line: Option<usize>,
}

impl Builder<'_> {
    fn push(&mut self, kind: Kind, s: usize, e: usize) {
        self.out.toks.push(Tok {
            kind,
            start: s as u32,
            end: e as u32,
            link: NO_LINK,
            popped: false,
        });
        self.hw = self.hw.max(e);
    }

    fn error(&mut self, s: usize, e: usize, kind: ErrorKind, message: String) {
        let token = self.out.toks.len() as u32;
        let line = line_start(self.src, s);
        // Errors arrive in source order, so an earlier error on this line is
        // the last one kept.
        let publish = self.last_error_line != Some(line);
        self.last_error_line = Some(line);
        self.push(Kind::Error(kind), s, e);
        self.out.errors.push(LexError {
            token,
            message,
            publish,
        });
    }

    /// End the current segment at the line holding `s`: an error from `s` to
    /// the line end, then the offset of the next line (where lexing restarts).
    fn cut_line(&mut self, s: usize, kind: ErrorKind, message: String) -> usize {
        let end = line_end(self.src, s);
        let s = s.max(self.hw);
        if s < end {
            self.error(s, end, kind, message);
        }
        self.hw = self.hw.max(end);
        end + 1
    }

    /// Link every bracket: a closer matches the nearest opener of its kind on
    /// the stack, popping (and so ending) any opener above it; a closer that
    /// matches nothing is left unlinked.
    fn link(&mut self) {
        let mut stack: Vec<usize> = Vec::new();
        for i in 0..self.out.toks.len() {
            let Some(tok) = self.out.toks.get(i).copied() else {
                continue;
            };
            match tok.kind {
                Kind::Open(_) => stack.push(i),
                Kind::Close(b) => {
                    let Some(depth) = stack.iter().rposition(|&j| {
                        self.out
                            .toks
                            .get(j)
                            .is_some_and(|t| t.kind == Kind::Open(b))
                    }) else {
                        continue;
                    };
                    for j in stack.drain(depth..).rev() {
                        if let Some(opener) = self.out.toks.get_mut(j) {
                            opener.link = i as u32;
                            opener.popped = opener.kind != Kind::Open(b);
                        }
                        if !self.out.toks.get(j).is_some_and(|t| t.popped)
                            && let Some(closer) = self.out.toks.get_mut(i)
                        {
                            closer.link = j as u32;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Lex `src` into the token array.
pub(crate) fn lex(src: &str) -> Tokens {
    let cap = item_cap(src.len());
    let mut b = Builder {
        src,
        out: Tokens::default(),
        hw: 0,
        last_error_line: None,
    };
    let mut base = 0usize;
    let mut restarts = 0usize;
    'segments: while base < src.len() {
        if restarts > MAX_RESTARTS {
            let s = base.max(b.hw);
            b.error(
                s,
                src.len(),
                ErrorKind::Other,
                "too many lexer errors; the rest of the file is not read".to_string(),
            );
            break;
        }
        let segment = src.get(base..).unwrap_or("");
        let codemap = CodeMap::new(String::new(), segment.to_owned());
        let lexer = Lexer::new(codemap.source(), &Dialect::Extended, codemap.clone());
        // An f-string being skipped: its start, and its nesting depth.
        let mut fstring: Option<(usize, usize)> = None;
        let mut last_error: Option<(usize, usize)> = None;
        for item in lexer {
            b.out.items += 1;
            if b.out.items > cap {
                let s = b.hw.min(src.len());
                if s < src.len() {
                    b.error(
                        s,
                        src.len(),
                        ErrorKind::Other,
                        "lexing stopped at the item cap".to_string(),
                    );
                }
                break 'segments;
            }
            match item {
                Ok((s, tok, e)) => {
                    let (s, e) = (s + base, e + base);
                    if let Some((fs, depth)) = fstring.as_mut() {
                        let end = line_end(src, *fs);
                        if s > end || e > end {
                            // No end on its line: cut it there, restart below.
                            let fs = *fs;
                            base = b.cut_line(
                                fs,
                                ErrorKind::UnterminatedString { triple: false },
                                "unfinished f-string literal".to_string(),
                            );
                            restarts += 1;
                            continue 'segments;
                        }
                        match tok {
                            Token::FStringStart(_) => *depth += 1,
                            Token::FStringEnd => {
                                *depth -= 1;
                                if *depth == 0 {
                                    let fs = *fs;
                                    fstring = None;
                                    if fs >= b.hw {
                                        b.push(Kind::Str, fs, e);
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if s < b.hw {
                        continue;
                    }
                    if matches!(tok, Token::FStringStart(_)) {
                        fstring = Some((s, 1));
                        continue;
                    }
                    if let Some(kind) = kind_of(&tok) {
                        b.push(kind, s, e);
                    }
                }
                Err(ex) => {
                    let err = ex.into_error();
                    let (s, e) = err.span().map_or((b.hw, b.hw), |fs| {
                        (
                            fs.span.begin().get() as usize + base,
                            fs.span.end().get() as usize + base,
                        )
                    });
                    let message = err
                        .without_diagnostic()
                        .to_string()
                        .trim_start_matches("Parse error: ")
                        .to_string();
                    if let Some((fs, _)) = fstring {
                        // An error inside an f-string: the whole f-string is
                        // broken to its line end.
                        base = b.cut_line(
                            fs,
                            ErrorKind::UnterminatedString { triple: false },
                            message,
                        );
                        restarts += 1;
                        continue 'segments;
                    }
                    if last_error == Some((s, e)) {
                        // No progress: the lexer repeats itself.
                        base = b.cut_line(s, ErrorKind::Other, message);
                        restarts += 1;
                        continue 'segments;
                    }
                    last_error = Some((s, e));
                    if s < b.hw {
                        continue;
                    }
                    // A fresh lexer mid-file measures indentation from its own
                    // first line, so its indentation complaints are artefacts.
                    if base > 0 && message.contains("indentation") {
                        continue;
                    }
                    let kind = classify_error(src, s, e);
                    b.error(s, e, kind, message);
                }
            }
        }
        if let Some((fs, _)) = fstring {
            // The lexer ended inside an f-string.
            base = b.cut_line(
                fs,
                ErrorKind::UnterminatedString { triple: false },
                "unfinished f-string literal".to_string(),
            );
            restarts += 1;
            continue 'segments;
        }
        break;
    }
    b.link();
    b.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Kind, &str)> {
        let t = lex(src);
        t.toks
            .iter()
            .map(|tok| (tok.kind, src.get(tok.range()).unwrap_or("?")))
            .collect()
    }

    #[test]
    fn unterminated_string_drops_its_relexed_body() {
        let src = "target(driver = \"ex)\nname = 1\n";
        let toks = kinds(src);
        let unterminated = Kind::Error(ErrorKind::UnterminatedString { triple: false });
        assert!(toks.contains(&(unterminated, "\"ex)")), "{toks:?}");
        assert!(
            !toks.iter().any(|(k, s)| *k == Kind::Ident && *s == "ex"),
            "the body is not re-lexed: {toks:?}"
        );
        assert!(
            !toks.contains(&(Kind::Close(Bracket::Round), ")")),
            "the swallowed paren is no closer: {toks:?}"
        );
        assert!(
            toks.contains(&(Kind::Ident, "name")),
            "next line lexes: {toks:?}"
        );
    }

    #[test]
    fn unterminated_fstring_terminates_and_restarts_on_the_next_line() {
        let src = "target(driver = f\"ex\nname = 1\n";
        let t = lex(src);
        assert!(t.items <= item_cap(src.len()));
        let toks = kinds(src);
        assert!(toks.contains(&(Kind::Ident, "name")), "{toks:?}");
    }

    #[test]
    fn brackets_link_and_mismatches_pop() {
        let src = "deps = [ target( ]\nfoo(]\n";
        let t = lex(src);
        let open_round = t
            .toks
            .iter()
            .position(|x| x.kind == Kind::Open(Bracket::Round));
        let tok = open_round.and_then(|i| t.toks.get(i)).copied().unwrap();
        assert!(tok.popped, "target( is popped by the ]");
        let foo = t
            .toks
            .iter()
            .rposition(|x| x.kind == Kind::Open(Bracket::Round))
            .and_then(|i| t.toks.get(i))
            .copied()
            .unwrap();
        assert_eq!(
            foo.link, NO_LINK,
            "foo( never closes: the ] matched nothing"
        );
    }
}

/// The raw `starlark_syntax` lexer on the 24 broken inputs the design was
/// probed with, item by item, against a golden file. A `starlark_syntax` bump
/// that changes any behaviour the loop above compensates for fails here first.
#[cfg(test)]
mod probe {
    use starlark_syntax::codemap::CodeMap;
    use starlark_syntax::lexer::Lexer;
    use starlark_syntax::syntax::Dialect;

    /// The inputs, as probed during design (`U` loops forever on 0.14.2, so
    /// every input is capped).
    const INPUTS: &[(&str, &str)] = &[
        (
            "A unterminated string, paren on next line",
            "target(name = \"x\", driver = \"ex\n)\n",
        ),
        ("B lex error before cursor", "x = \"oops\ntarget(dri\n"),
        ("C missing comma", "target(\n  name = \"x\"\n  dri\n)\n"),
        ("D unclosed at EOF", "target(dri"),
        (
            "E bad indentation before cursor",
            "if x:\n        a = 1\n    b = 2\ntarget(dri\n",
        ),
        ("F stray char", "target(name = $, dri\n"),
        (
            "G unterminated triple before cursor",
            "\"\"\"abc\ntarget(dri\n",
        ),
        (
            "H unterminated string swallowing paren at EOF",
            "target(name = \"x\", driver = \"ex)",
        ),
        ("I mismatched bracket before cursor", "foo(]\ntarget(dri\n"),
        (
            "J single quote unterminated with auto-pair text after",
            "target(driver = 'ex\nbar = 1\n",
        ),
        ("K bad escape", "target(driver = \"\\q\", na\n"),
        ("L tab indent", "if x:\n\ta = 1\ntarget(dri\n"),
        (
            "M invalid escape in a closed string",
            "target(driver = \"ex\\xZZ\", na\n",
        ),
        ("O raw prefix unterminated", "target(driver = r\"ex\n)\n"),
        ("P bytes prefix unterminated", "target(driver = b\"ex\n)\n"),
        ("Q lone quote as last byte", "target(driver = \""),
        ("R CRLF after unterminated", "target(driver = \"ex\r\n)\r\n"),
        (
            "S quote inside a comment",
            "x = 1  # say \"hi\ntarget(dri\n",
        ),
        (
            "T non-ascii before cursor",
            "target(name = \"caf\u{e9}\", driver = \"ex\n",
        ),
        ("U f-string unterminated", "target(driver = f\"ex\n)\n"),
        (
            "W two errors on one line",
            "target(name = $, driver = \"ex\n",
        ),
        (
            "X unterminated string inside a nested list",
            "target(srcs = [\"//a:b, dri\n",
        ),
        (
            "Y triple quote closed then cursor",
            "\"\"\"doc\"\"\"\ntarget(dri\n",
        ),
        (
            "Z unterminated triple with cursor above it",
            "target(dri\n\"\"\"abc\n",
        ),
    ];

    /// Items rendered per input before the cap.
    const ITEM_CAP: usize = 40;

    const GOLDEN: &str = include_str!("lexer_probe.golden");

    fn render() -> String {
        let mut out = String::new();
        for (name, src) in INPUTS {
            out.push_str(&format!("=== {name}: {src:?}\n"));
            let cm = CodeMap::new("f".to_owned(), (*src).to_owned());
            let lexer = Lexer::new(cm.source(), &Dialect::Extended, cm.clone());
            let mut n = 0;
            for item in lexer {
                n += 1;
                if n > ITEM_CAP {
                    out.push_str("  ... (capped)\n");
                    break;
                }
                match item {
                    Ok((s, t, e)) => out.push_str(&format!("  {s}..{e} {t:?}\n")),
                    Err(ex) => {
                        let err = ex.into_error();
                        let span = err
                            .span()
                            .map(|s| format!("{}..{}", s.span.begin().get(), s.span.end().get()))
                            .unwrap_or_default();
                        out.push_str(&format!("  ERR {span} {}\n", err.without_diagnostic()));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn lexer_probe_table_pinned_to_starlark_syntax_version() {
        let actual = render();
        if std::env::var_os("HEPH_UPDATE_LEXER_PROBE").is_some() {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/pluginbuildfile/lsp/lexer_probe.golden"
            );
            std::fs::write(path, &actual).expect("write golden");
            return;
        }
        assert!(
            actual == GOLDEN,
            "starlark_syntax's lexer changed behaviour on the probed inputs; review \
             the loop in lex.rs against the new output, then regenerate with \
             HEPH_UPDATE_LEXER_PROBE=1.\n--- actual ---\n{actual}"
        );
    }
}

#[cfg(test)]
mod sweep {
    use super::*;

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/lsp_fixtures.rs"
    ));

    /// Every byte prefix of every fixture lexes within the item cap: no panic,
    /// no hang, whatever the prefix cuts through.
    #[test]
    fn lexer_prefix_sweep_terminates_within_item_cap() {
        for (name, fixture, _) in FIXTURES {
            let text = fixture.replacen('│', "", 1);
            for end in 0..=text.len() {
                let Some(prefix) = text.get(..end) else {
                    continue;
                };
                let tokens = lex(prefix);
                assert!(
                    tokens.items <= item_cap(prefix.len()),
                    "{name}: {} items for {} bytes",
                    tokens.items,
                    prefix.len()
                );
            }
        }
        // And the pathological inputs the loop exists for, unprefixed.
        for src in [
            "x = f\"ex",
            "x = f\"ex\n",
            "x = f\"a{b",
            "x = f'''abc\ny\n",
            "\"\"\"",
            "f\"\"\"{",
        ] {
            let tokens = lex(src);
            assert!(tokens.items <= item_cap(src.len()), "{src:?}");
        }
    }

    /// The boundary rule per error kind, at the span's start, start+1, end−1
    /// and end: inside an unterminated string is `(start, end]`; inside any
    /// other error is the open interval; a zero-width span contains nothing.
    #[test]
    fn error_span_boundaries_by_kind() {
        let find = |src: &str, kind: fn(&Kind) -> bool| -> Tok {
            lex(src)
                .toks
                .into_iter()
                .find(|t| kind(&t.kind))
                .unwrap_or_else(|| panic!("no such token in {src:?}"))
        };
        // Unterminated string: `"ex` at 16..19.
        let t = find("target(driver = \"ex\n", |k| {
            matches!(k, Kind::Error(ErrorKind::UnterminatedString { .. }))
        });
        let (s, e) = (t.start as usize, t.end as usize);
        assert!(
            !t.contains(s),
            "at its start the cursor is before the quote"
        );
        assert!(t.contains(s + 1));
        assert!(t.contains(e - 1));
        assert!(
            t.contains(e),
            "at its end the cursor is still in the string"
        );

        // Another error: the stray `$`, two bytes wide here (`$$`).
        let t = find("x = $$\n", |k| matches!(k, Kind::Error(ErrorKind::Other)));
        let (s, e) = (t.start as usize, t.end as usize);
        assert!(!t.contains(s));
        assert!(t.contains(s + 1).eq(&(s + 1 < e)));
        assert!(!t.contains(e));

        // A wider other error: the bad escape `\xZ`.
        let t = find("x = \"a\\xZZ\"\n", |k| {
            matches!(k, Kind::Error(ErrorKind::Other))
        });
        let (s, e) = (t.start as usize, t.end as usize);
        assert!(e - s >= 3, "{t:?}");
        assert!(!t.contains(s));
        assert!(t.contains(s + 1));
        assert!(t.contains(e - 1));
        assert!(!t.contains(e));

        // Zero-width: the tab error at an indented line's start.
        let t = find("if x:\n\ta = 1\n", |k| matches!(k, Kind::Error(_)));
        assert_eq!(t.start, t.end, "{t:?}");
        for o in [t.start as usize, t.start as usize + 1] {
            assert!(!t.contains(o));
        }
    }
}
