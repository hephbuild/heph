//! Query language for selecting targets.
//!
//! Parses an expression like `//some/... && label(foo)` into a [`Matcher`].
//! Every [`Matcher`] variant is reachable:
//!
//! - bare patterns — `//pkg` ([`Matcher::Package`]), `//pkg/...`
//!   ([`Matcher::PackagePrefix`]), `//pkg:name` ([`Matcher::Addr`]); relative
//!   forms (`./x`, `../x`, `.`, `..`) resolve against the base package.
//! - functions — `label(x)`, `driver(name)`, `tree_output(pkg)`, plus the
//!   explicit `addr(x)`, `package(x)`, `package_prefix(x)` forms. `label`'s
//!   argument must be a well-formed label (`[A-Za-z0-9_-]+`, see
//!   [`crate::htlabel`]); anything else is a parse error rather than a label
//!   nothing carries. `driver` ([`Matcher::Driver`]) compares the spec's driver
//!   whole and case-sensitively, and needs a non-empty name.
//! - function arguments — every argument is either a bare word or a `"…"`
//!   string literal with escapes (`\\`, `\"`, `\n`, `\r`, `\t`, `\u{HEX}`),
//!   with identical meaning: `label(ci)` is `label("ci")`. Validation applies
//!   to the unquoted text, so `label("a b")` is still an error. Escapes apply
//!   only to a function argument: a quoted bare pattern (`"//foo"`) reads as a
//!   plain word, a `\` in it staying a `\`.
//! - operators — `&&` ([`Matcher::And`]), `||` ([`Matcher::Or`]),
//!   `!` ([`Matcher::Not`]), and `( … )` grouping.
//!
//! Precedence is `!` > `&&` > `||`; same-precedence chains keep source order so
//! the engine's short-circuit evaluation runs left-to-right and bails early.
//!
//! See [`parse`] for the entry point.

use crate::htaddr::parse_addr_with_base;
use crate::htlabel;
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf, join_rel_checked_pkg};
use anyhow::{Context, Result, bail};

/// Render a [`Matcher`] back into query-language syntax — the inverse of
/// [`parse`]. Round-trips (re-parsing the output yields an equivalent matcher),
/// with parentheses inserted only where precedence (`!` > `&&` > `||`) demands.
/// Used for human-facing display (e.g. the TUI progress label).
pub fn format(m: &Matcher) -> String {
    let mut out = String::new();
    fmt_prec(m, 0, &mut out);
    out
}

/// Binding strength used to decide parenthesisation: `||` binds loosest, then
/// `&&`, then unary/atoms.
fn prec(m: &Matcher) -> u8 {
    match m {
        Matcher::Or(_) => 1,
        Matcher::And(_) => 2,
        _ => 3,
    }
}

fn fmt_prec(m: &Matcher, parent_prec: u8, out: &mut String) {
    let needs_paren = prec(m) < parent_prec;
    if needs_paren {
        out.push('(');
    }
    match m {
        // A bare pattern when it tokenizes as one word; otherwise the explicit
        // function form, whose argument can be quoted.
        Matcher::Addr(a) => fmt_pattern(&a.format(), "addr", &a.format(), out),
        Matcher::Package(p) => {
            fmt_pattern(&format!("//{}", p.as_str()), "package", p.as_str(), out);
        }
        Matcher::PackagePrefix(p) => {
            let bare = if p.as_str().is_empty() {
                "//...".to_string()
            } else {
                format!("//{}/...", p.as_str())
            };
            fmt_pattern(&bare, "package_prefix", p.as_str(), out);
        }
        Matcher::Label(l) => fmt_call("label", l, out),
        Matcher::Driver(d) => fmt_call(DRIVER_FN, d, out),
        Matcher::TreeOutputTo(p) => fmt_call("tree_output", p.as_str(), out),
        Matcher::And(children) => fmt_join(children, " && ", 2, out),
        Matcher::Or(children) => fmt_join(children, " || ", 1, out),
        Matcher::Not(inner) => {
            out.push('!');
            fmt_prec(inner, 3, out);
        }
    }
    if needs_paren {
        out.push(')');
    }
}

/// `bare` as is when it re-tokenizes as that one word, else `func("arg")`.
fn fmt_pattern(bare: &str, func: &str, arg: &str, out: &mut String) {
    if is_bare_word(bare) {
        out.push_str(bare);
    } else {
        fmt_call(func, arg, out);
    }
}

/// `name(arg)`, the argument bare when it re-tokenizes as that same single
/// word and quoted otherwise. An empty argument renders as `name()`.
fn fmt_call(name: &str, arg: &str, out: &mut String) {
    out.push_str(name);
    out.push('(');
    fmt_arg(arg, out);
    out.push(')');
}

fn fmt_arg(arg: &str, out: &mut String) {
    if arg.is_empty() || is_bare_word(arg) {
        out.push_str(arg);
    } else {
        quote_string(arg, out);
    }
}

/// `name(arg)` as [`format`] renders it, for error messages.
fn call_display(name: &str, arg: &str) -> String {
    let mut out = String::new();
    fmt_call(name, arg, &mut out);
    out
}

/// Whether [`tokenize`] reads `s` back as exactly one bare word with this
/// text. Control characters are excluded too, so a rendered query never
/// carries a raw one.
fn is_bare_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| !is_word_break(c) && !c.is_control())
}

/// A character that ends a bare word.
fn is_word_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '&' | '|' | '!' | '"')
}

fn fmt_join(children: &[Matcher], sep: &str, child_prec: u8, out: &mut String) {
    for (i, c) in children.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        fmt_prec(c, child_prec, out);
    }
}

/// Parse a query expression into a [`Matcher`]. Relative patterns resolve
/// against `base` (the current working package).
pub fn parse(input: &str, base: &PkgBuf) -> Result<Matcher> {
    let tokens = tokenize(input).context("tokenizing query")?;
    let mut p = Parser {
        tokens: &tokens,
        pos: 0,
        base,
        depth: 0,
    };
    let m = p.parse_or()?;
    if let Some(tok) = p.peek() {
        bail!("unexpected `{}` in query", tok.describe());
    }
    Ok(m)
}

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    And,
    Or,
    Not,
    LParen,
    RParen,
    /// A pattern or function-name token (e.g. `//foo:bar`, `label`).
    Word(String),
    /// A string literal with its escapes resolved — only ever a function's
    /// argument.
    Str(String),
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::And => "&&".to_string(),
            Tok::Or => "||".to_string(),
            Tok::Not => "!".to_string(),
            Tok::LParen => "(".to_string(),
            Tok::RParen => ")".to_string(),
            Tok::Word(w) => w.clone(),
            Tok::Str(s) => {
                let mut out = String::new();
                quote_string(s, &mut out);
                out
            }
        }
    }
}

/// Split a query into tokens. Words are runs of any character except
/// whitespace and the metacharacters `( ) & | ! "`. A function argument may be
/// a `"…"` string literal with escapes ([`Tok::Str`]); elsewhere `"…"` wraps a
/// plain word with no escapes, as it always has.
fn tokenize(input: &str) -> Result<Vec<Tok>> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Tok::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Tok::RParen);
            }
            '!' => {
                chars.next();
                tokens.push(Tok::Not);
            }
            '&' => {
                chars.next();
                if chars.next_if_eq(&'&').is_none() {
                    bail!("expected `&&`, found single `&`");
                }
                tokens.push(Tok::And);
            }
            '|' => {
                chars.next();
                if chars.next_if_eq(&'|').is_none() {
                    bail!("expected `||`, found single `|`");
                }
                tokens.push(Tok::Or);
            }
            // A function's argument is a string literal with escapes. A word
            // followed by `(` is always a call to the parser, so this is
            // exactly the argument position; every other quoted word (a bare
            // pattern) tokenizes as it always did — a `\` in one stays a `\`.
            '"' if matches!(tokens.as_slice(), [.., Tok::Word(_), Tok::LParen]) => {
                chars.next();
                tokens.push(Tok::Str(unescape_string(&mut chars)?));
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                let mut closed = false;
                for ch in chars.by_ref() {
                    if ch == '"' {
                        closed = true;
                        break;
                    }
                    s.push(ch);
                }
                if !closed {
                    bail!("unterminated quoted string in query");
                }
                tokens.push(Tok::Word(s));
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if is_word_break(c) {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                tokens.push(Tok::Word(s));
            }
        }
    }
    Ok(tokens)
}

const DRIVER_FN: &str = "driver";

/// The body of a `"…"` literal after its opening quote, through the closing
/// one. Escapes: `\\`, `\"`, `\n`, `\r`, `\t` and `\u{HEX}`; anything else
/// after a `\` is an error rather than a guess.
fn unescape_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String> {
    let mut s = String::new();
    loop {
        match chars.next() {
            None => bail!("unterminated string literal in query"),
            Some('"') => return Ok(s),
            Some('\\') => match chars.next() {
                Some('\\') => s.push('\\'),
                Some('"') => s.push('"'),
                Some('n') => s.push('\n'),
                Some('r') => s.push('\r'),
                Some('t') => s.push('\t'),
                Some('u') => {
                    if chars.next() != Some('{') {
                        bail!("expected `{{` after `\\u` in string literal");
                    }
                    let mut hex = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) if c.is_ascii_hexdigit() && hex.len() < 6 => hex.push(c),
                            _ => bail!("malformed `\\u{{…}}` escape in string literal"),
                        }
                    }
                    let c = u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .with_context(|| format!("`\\u{{{hex}}}` is not a character"))?;
                    s.push(c);
                }
                Some(c) => bail!("unknown escape `\\{c}` in string literal"),
                None => bail!("unterminated string literal in query"),
            },
            Some(c) => s.push(c),
        }
    }
}

/// `s` as a `"…"` literal [`unescape_string`] reads back unchanged. Control
/// characters are escaped so a rendered query never carries a raw one.
fn quote_string(s: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                // Writing to a `String` cannot fail.
                write!(out, "\\u{{{:x}}}", u32::from(c)).unwrap_or_default();
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Cap on `!`/`(…)` nesting depth. Each level of nesting descends through
/// `parse_not`, so this bounds native stack usage — a deeply nested or
/// parenthesized expression must fail cleanly instead of overflowing the
/// stack (reachable from `heph query -e '<expr>'` on user-controlled input).
const MAX_NESTING_DEPTH: usize = 256;

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
    base: &'a PkgBuf,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&'a Tok> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// `or := and ( "||" and )*`
    fn parse_or(&mut self) -> Result<Matcher> {
        let first = self.parse_and()?;
        if !matches!(self.peek(), Some(Tok::Or)) {
            return Ok(first);
        }
        let mut terms = vec![first];
        while matches!(self.peek(), Some(Tok::Or)) {
            self.bump();
            terms.push(self.parse_and()?);
        }
        Ok(Matcher::Or(terms))
    }

    /// `and := not ( "&&" not )*`
    fn parse_and(&mut self) -> Result<Matcher> {
        let first = self.parse_not()?;
        if !matches!(self.peek(), Some(Tok::And)) {
            return Ok(first);
        }
        let mut terms = vec![first];
        while matches!(self.peek(), Some(Tok::And)) {
            self.bump();
            terms.push(self.parse_not()?);
        }
        Ok(Matcher::And(terms))
    }

    /// `not := "!" not | atom`
    ///
    /// Every level of `!`/`(…)` nesting passes through here exactly once
    /// (directly for `!`, via `parse_atom` -> `parse_or` -> `parse_and` for
    /// `(…)`), so guarding depth here bounds both recursion paths.
    fn parse_not(&mut self) -> Result<Matcher> {
        self.depth += 1;
        let result = self.parse_not_inner();
        self.depth -= 1;
        result
    }

    fn parse_not_inner(&mut self) -> Result<Matcher> {
        if self.depth > MAX_NESTING_DEPTH {
            bail!("query expression nested too deeply (limit: {MAX_NESTING_DEPTH})");
        }
        if matches!(self.peek(), Some(Tok::Not)) {
            self.bump();
            return self.parse_not().map(|inner| Matcher::Not(Box::new(inner)));
        }
        self.parse_atom()
    }

    /// `atom := "(" or ")" | func | pattern`
    fn parse_atom(&mut self) -> Result<Matcher> {
        match self.bump() {
            Some(Tok::LParen) => {
                let inner = self.parse_or()?;
                match self.bump() {
                    Some(Tok::RParen) => Ok(inner),
                    Some(other) => bail!("expected `)`, found `{}`", other.describe()),
                    None => bail!("unclosed `(` in query"),
                }
            }
            Some(Tok::Word(w)) => {
                // A word immediately followed by `(` is a function call.
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.bump();
                    // An empty arg list (`tree_output()`) is allowed: it denotes
                    // the root package for the package-shaped functions.
                    let arg = if matches!(self.peek(), Some(Tok::RParen)) {
                        None
                    } else {
                        match self.bump() {
                            Some(Tok::Word(a) | Tok::Str(a)) => Some(a.as_str()),
                            Some(other) => {
                                bail!("expected argument to `{w}()`, found `{}`", other.describe())
                            }
                            None => bail!("missing argument to `{w}()`"),
                        }
                    };
                    match self.bump() {
                        Some(Tok::RParen) => {}
                        Some(other) => {
                            bail!(
                                "expected `)` after `{}`, found `{}`",
                                call_display(w, arg.unwrap_or_default()),
                                other.describe()
                            )
                        }
                        None => bail!("unclosed `(` in `{w}(`"),
                    }
                    if w == DRIVER_FN {
                        let Some(arg) = arg.filter(|a| !a.is_empty()) else {
                            bail!(
                                "`driver()` needs a non-empty driver name, e.g. `driver(auth.credential)` or `driver(\"my driver\")`"
                            );
                        };
                        return Ok(Matcher::Driver(arg.to_string()));
                    }
                    self.func_to_matcher(w, arg.unwrap_or_default())
                } else {
                    self.pattern_to_matcher(w)
                }
            }
            Some(other) => bail!("unexpected `{}` in query", other.describe()),
            None => bail!("unexpected end of query, expected a pattern"),
        }
    }

    /// `arg` is the unquoted text: a bare word and a quoted string reach here
    /// alike, and are validated alike.
    fn func_to_matcher(&self, name: &str, arg: &str) -> Result<Matcher> {
        let ctx = || format!("parsing {}", call_display(name, arg));
        match name {
            "label" => {
                htlabel::validate(arg).with_context(ctx)?;
                Ok(Matcher::Label(arg.to_string()))
            }
            "tree_output" | "tree_output_to" => {
                Ok(Matcher::TreeOutputTo(to_pkg(arg).with_context(ctx)?))
            }
            "addr" => Ok(Matcher::Addr(
                parse_addr_with_base(arg, self.base).with_context(ctx)?,
            )),
            "package" | "pkg" => Ok(Matcher::Package(to_pkg(arg).with_context(ctx)?)),
            "package_prefix" => Ok(Matcher::PackagePrefix(to_pkg(arg).with_context(ctx)?)),
            other => bail!(
                "unknown query function `{other}` (expected one of: label, tree_output, addr, package, package_prefix, driver; each takes a bare or quoted argument)"
            ),
        }
    }

    /// A bare pattern: `//pkg:name` is an [`Matcher::Addr`]; anything without a
    /// `:` is a package pattern (`//pkg`, `//pkg/...`, `./x`, `.`).
    fn pattern_to_matcher(&self, word: &str) -> Result<Matcher> {
        if word.contains(':') {
            Ok(Matcher::Addr(
                parse_addr_with_base(word, self.base)
                    .with_context(|| format!("parsing `{word}`"))?,
            ))
        } else {
            htpkg::parse(word, self.base).with_context(|| format!("parsing `{word}`"))
        }
    }
}

/// Normalise a package argument: strip a leading `//` so both `//pkg` and `pkg`
/// are accepted, and bound `..` segments to the workspace root (same rule
/// `parse_addr` enforces for `//pkg:name`).
fn to_pkg(arg: &str) -> Result<PkgBuf> {
    let arg = arg.strip_prefix("//").unwrap_or(arg);
    Ok(PkgBuf::from(join_rel_checked_pkg("", arg)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PkgBuf {
        PkgBuf::from("cwd/pkg")
    }

    fn p(input: &str) -> Matcher {
        parse(input, &base()).expect("parse")
    }

    #[test]
    fn bare_package() {
        assert_eq!(p("//foo/bar"), Matcher::Package(PkgBuf::from("foo/bar")));
    }

    #[test]
    fn bare_package_prefix() {
        assert_eq!(p("//foo/..."), Matcher::PackagePrefix(PkgBuf::from("foo")));
    }

    #[test]
    fn bare_addr() {
        match p("//foo:bar") {
            Matcher::Addr(a) => {
                assert_eq!(a.package.as_str(), "foo");
                assert_eq!(a.name, "bar");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn relative_addr_resolves_base() {
        match p(":bar") {
            Matcher::Addr(a) => {
                assert_eq!(a.package.as_str(), "cwd/pkg");
                assert_eq!(a.name, "bar");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn relative_package_resolves_base() {
        assert_eq!(p("./sub"), Matcher::Package(PkgBuf::from("cwd/pkg/sub")));
        assert_eq!(p("."), Matcher::Package(PkgBuf::from("cwd/pkg")));
    }

    #[test]
    fn label_func() {
        assert_eq!(p("label(foo)"), Matcher::Label("foo".to_string()));
        assert_eq!(p("label(go-lint)"), Matcher::Label("go-lint".to_string()));
        assert_eq!(p("label(go_lint2)"), Matcher::Label("go_lint2".to_string()));
    }

    #[test]
    fn err_label_outside_the_grammar() {
        // A label is `[A-Za-z0-9_-]+`. Address-shaped and quoted-with-spaces
        // forms used to parse into a label no target could ever carry.
        for src in [
            "label(//tag:release)",
            "label(\"my label\")",
            "label()",
            "label(caf\u{e9})",
        ] {
            let err = parse(src, &base())
                .err()
                .unwrap_or_else(|| panic!("{src} should not parse"));
            let chain = format!("{err:#}");
            assert!(chain.contains("label"), "{chain}");
        }
    }

    #[test]
    fn tree_output_func() {
        assert_eq!(
            p("tree_output(some/pkg/deep)"),
            Matcher::TreeOutputTo(PkgBuf::from("some/pkg/deep"))
        );
        // `//` prefix accepted and stripped.
        assert_eq!(
            p("tree_output(//some/pkg)"),
            Matcher::TreeOutputTo(PkgBuf::from("some/pkg"))
        );
    }

    #[test]
    fn explicit_funcs() {
        assert_eq!(p("package(foo)"), Matcher::Package(PkgBuf::from("foo")));
        assert_eq!(
            p("package_prefix(foo)"),
            Matcher::PackagePrefix(PkgBuf::from("foo"))
        );
        match p("addr(//foo:bar)") {
            Matcher::Addr(a) => assert_eq!(a.name, "bar"),
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn and_or_precedence() {
        // `a && b || c` => Or([And([a, b]), c])
        let m = p("//a && //b || //c");
        assert_eq!(
            m,
            Matcher::Or(vec![
                Matcher::And(vec![
                    Matcher::Package(PkgBuf::from("a")),
                    Matcher::Package(PkgBuf::from("b")),
                ]),
                Matcher::Package(PkgBuf::from("c")),
            ])
        );
    }

    #[test]
    fn and_chain_flattens_in_order() {
        let m = p("//a && //b && //c");
        assert_eq!(
            m,
            Matcher::And(vec![
                Matcher::Package(PkgBuf::from("a")),
                Matcher::Package(PkgBuf::from("b")),
                Matcher::Package(PkgBuf::from("c")),
            ])
        );
    }

    #[test]
    fn grouping_overrides_precedence() {
        // `a && (b || c)` => And([a, Or([b, c])])
        let m = p("//a && (//b || //c)");
        assert_eq!(
            m,
            Matcher::And(vec![
                Matcher::Package(PkgBuf::from("a")),
                Matcher::Or(vec![
                    Matcher::Package(PkgBuf::from("b")),
                    Matcher::Package(PkgBuf::from("c")),
                ]),
            ])
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        // `!a && b` => And([Not(a), b])
        let m = p("!//a && //b");
        assert_eq!(
            m,
            Matcher::And(vec![
                Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("a")))),
                Matcher::Package(PkgBuf::from("b")),
            ])
        );
    }

    #[test]
    fn not_group() {
        let m = p("!(//a || //b)");
        assert_eq!(
            m,
            Matcher::Not(Box::new(Matcher::Or(vec![
                Matcher::Package(PkgBuf::from("a")),
                Matcher::Package(PkgBuf::from("b")),
            ])))
        );
    }

    #[test]
    fn double_not() {
        let m = p("!!//a");
        assert_eq!(
            m,
            Matcher::Not(Box::new(Matcher::Not(Box::new(Matcher::Package(
                PkgBuf::from("a")
            )))))
        );
    }

    #[test]
    fn combined_example() {
        // The motivating example from the feature request.
        let m = p("//some/... && label(foo)");
        assert_eq!(
            m,
            Matcher::And(vec![
                Matcher::PackagePrefix(PkgBuf::from("some")),
                Matcher::Label("foo".to_string()),
            ])
        );
    }

    #[test]
    fn format_atoms() {
        assert_eq!(format(&p("//foo/bar")), "//foo/bar");
        assert_eq!(format(&p("//foo/...")), "//foo/...");
        assert_eq!(format(&p("//...")), "//...");
        assert_eq!(format(&p("//foo:bar")), "//foo:bar");
        assert_eq!(format(&p("label(test)")), "label(test)");
        assert_eq!(format(&p("tree_output(gen)")), "tree_output(gen)");
    }

    #[test]
    fn format_inserts_parens_only_where_needed() {
        // && binds tighter than || → no parens needed here.
        assert_eq!(format(&p("//a && //b || //c")), "//a && //b || //c");
        // grouping that overrides precedence must be preserved.
        assert_eq!(format(&p("//a && (//b || //c)")), "//a && (//b || //c)");
        // ! over a group keeps the parens.
        assert_eq!(format(&p("!(//a || //b)")), "!(//a || //b)");
        // ! over an atom needs none.
        assert_eq!(format(&p("!//a")), "!//a");
    }

    #[test]
    fn format_round_trips() {
        let base = base();
        for src in [
            "//foo/... && label(test)",
            "//a && //b || //c",
            "//a && (//b || //c)",
            "!(//a || //b) && //c",
            "//app/... && !label(slow)",
            "(//a/... || //b/...) && tree_output(gen)",
        ] {
            let m1 = parse(src, &base).expect("parse src");
            let rendered = format(&m1);
            let m2 = parse(&rendered, &base).expect("parse rendered");
            assert_eq!(m1, m2, "round-trip mismatch: {src:?} -> {rendered:?}");
        }
    }

    #[test]
    fn whitespace_insensitive() {
        let tight = p("//a&&//b");
        let loose = p("  //a   &&   //b  ");
        assert_eq!(tight, loose);
    }

    #[test]
    fn err_empty() {
        assert!(parse("", &base()).is_err());
    }

    #[test]
    fn err_trailing_operator() {
        assert!(parse("//a &&", &base()).is_err());
    }

    #[test]
    fn err_unclosed_paren() {
        assert!(parse("(//a", &base()).is_err());
    }

    #[test]
    fn err_unexpected_close_paren() {
        assert!(parse("//a)", &base()).is_err());
    }

    #[test]
    fn err_single_ampersand() {
        assert!(parse("//a & //b", &base()).is_err());
    }

    #[test]
    fn err_unknown_function() {
        assert!(parse("bogus(foo)", &base()).is_err());
    }

    #[test]
    fn err_unterminated_quote() {
        assert!(parse("label(\"foo)", &base()).is_err());
        assert!(parse("\"//foo", &base()).is_err());
    }

    #[test]
    fn empty_func_arg_means_root_pkg() {
        assert_eq!(p("tree_output()"), Matcher::TreeOutputTo(PkgBuf::from("")));
        assert_eq!(p("package()"), Matcher::Package(PkgBuf::from("")));
        assert_eq!(
            p("package_prefix()"),
            Matcher::PackagePrefix(PkgBuf::from(""))
        );
    }

    #[test]
    fn err_empty_addr_func_arg() {
        // `addr()` still needs a real address.
        assert!(parse("addr()", &base()).is_err());
    }

    #[test]
    fn err_deeply_nested_not_returns_clean_error() {
        // Reachable via `heph query -e '<expr>'` on user-controlled input;
        // must fail cleanly instead of stack-overflowing the process.
        let input = format!("{}//a:b", "!".repeat(10_000));
        assert!(parse(&input, &base()).is_err());
    }

    #[test]
    fn err_deeply_nested_parens_returns_clean_error() {
        let input = format!("{}//a:b{}", "(".repeat(10_000), ")".repeat(10_000));
        assert!(parse(&input, &base()).is_err());
    }

    #[test]
    fn err_package_func_dotdot_escapes_root_fails() {
        // package()/package_prefix()/tree_output() args funnel through the
        // same PkgBuf construction as bare patterns — must reject `..` past
        // the workspace root too.
        assert!(parse("package(../../etc)", &base()).is_err());
        assert!(parse("package_prefix(../../etc)", &base()).is_err());
        assert!(parse("tree_output(../../etc)", &base()).is_err());
    }

    /// Every function takes its argument bare or quoted, with one meaning.
    #[test]
    fn every_function_accepts_bare_and_quoted_arguments() {
        for (bare, quoted) in [
            ("label(ci)", "label(\"ci\")"),
            ("driver(bash)", "driver(\"bash\")"),
            ("tree_output(foo/bar)", "tree_output(\"foo/bar\")"),
            ("tree_output_to(foo/bar)", "tree_output_to(\"foo/bar\")"),
            ("addr(//p:x)", "addr(\"//p:x\")"),
            ("addr(:x)", "addr(\":x\")"),
            ("package(//foo)", "package(\"//foo\")"),
            ("pkg(foo)", "pkg(\"foo\")"),
            ("package_prefix(foo)", "package_prefix(\"foo\")"),
        ] {
            assert_eq!(p(bare), p(quoted), "{bare} vs {quoted}");
            // Whitespace inside the parentheses is not part of either form.
            let spaced = quoted.replacen('(', "( ", 1).replacen(')', " )", 1);
            assert_eq!(p(bare), p(&spaced), "{bare} vs {spaced}");
        }
        assert_eq!(p("label(\"ci\")"), Matcher::Label("ci".to_string()));
        // Escapes resolve in every function's argument, not just `driver`'s.
        assert_eq!(p("label(\"\\u{63}i\")"), Matcher::Label("ci".to_string()));
        assert_eq!(
            p("driver(\"\\u{41}\\\"\\\\\")"),
            Matcher::Driver("A\"\\".to_string())
        );
        assert_eq!(
            p("tree_output(\"a b\\\\c\")"),
            Matcher::TreeOutputTo(PkgBuf::from("a b\\c"))
        );
        // `tree_output("")` is the root package, as `tree_output()` is.
        assert_eq!(p("tree_output(\"\")"), p("tree_output()"));
    }

    /// Validation runs on the unquoted text, exactly as on a bare word.
    #[test]
    fn quoting_does_not_bypass_validation() {
        for src in [
            "label(\"bad label\")",
            "label(\"\")",
            "label(\"a\\u{20}b\")",
            "addr(\"//p\")",
            "package(\"../../etc\")",
        ] {
            assert!(parse(src, &base()).is_err(), "{src} should not parse");
        }
        let err = parse("label(\"bad label\")", &base()).expect_err("bad label");
        assert!(
            format!("{err:#}").contains("label(\"bad label\")"),
            "{err:#}"
        );
    }

    #[test]
    fn format_renders_arguments_bare_when_possible() {
        assert_eq!(format(&p("driver(\"bash\")")), "driver(bash)");
        assert_eq!(format(&p("label(\"ci\")")), "label(ci)");
        assert_eq!(
            format(&p("tree_output(\"foo/bar\")")),
            "tree_output(foo/bar)"
        );
        assert_eq!(format(&p("addr(\"//p:x\")")), "//p:x");
        assert_eq!(format(&p("tree_output()")), "tree_output()");
        assert_eq!(
            format(&Matcher::Driver("a b".to_string())),
            "driver(\"a b\")"
        );
        assert_eq!(
            format(&Matcher::Package(PkgBuf::from("a b"))),
            "package(\"a b\")"
        );
    }

    /// `parse(format(m)) == m` for every function, over arguments that need
    /// quoting and ones that don't.
    #[test]
    fn format_round_trips_every_function() {
        let base = base();
        let texts = [
            "plain",
            "foo/bar",
            "with space",
            "quote\"inside",
            "back\\slash",
            "trailing\\",
            "close)paren",
            "open(paren",
            "a,b",
            "amp&and|pipe!bang",
            "caf\u{e9} \u{1F600}",
            "ctl\u{1}\u{7f}\n\t\r",
            "label(x)",
        ];
        let mut cases: Vec<Matcher> = vec![
            Matcher::Label("ci".to_string()),
            Matcher::Label("go-lint_2".to_string()),
            Matcher::TreeOutputTo(PkgBuf::from("")),
            Matcher::Package(PkgBuf::from("")),
            Matcher::PackagePrefix(PkgBuf::from("")),
        ];
        for t in texts {
            cases.push(Matcher::Driver(t.to_string()));
            cases.push(Matcher::TreeOutputTo(PkgBuf::from(t)));
            cases.push(Matcher::Package(PkgBuf::from(t)));
            cases.push(Matcher::PackagePrefix(PkgBuf::from(t)));
        }
        // An address name excludes ` `, `:`, `@` and `|`; an argument value
        // with a space is quoted in the address itself.
        for name in [
            "x",
            "a(b)",
            "x!y",
            "a&b",
            "q\"n",
            "back\\slash",
            "caf\u{e9}",
        ] {
            cases.push(Matcher::Addr(crate::htaddr::Addr::new(
                PkgBuf::from("p/q"),
                name.to_string(),
                Default::default(),
            )));
        }
        cases.push(Matcher::Addr(crate::htaddr::Addr::new(
            PkgBuf::from("p"),
            "x".to_string(),
            [("k".to_string(), "v w".to_string())].into_iter().collect(),
        )));
        for m in cases {
            let wrapped = Matcher::And(vec![
                Matcher::Not(Box::new(m.clone())),
                Matcher::Label("x".to_string()),
            ]);
            for m in [m, wrapped] {
                let rendered = format(&m);
                assert!(
                    !rendered.chars().any(|c| c.is_control()),
                    "raw control byte in {rendered:?}"
                );
                let back = parse(&rendered, &base)
                    .unwrap_or_else(|e| panic!("re-parsing {rendered:?}: {e:#}"));
                assert_eq!(back, m, "{m:?} -> {rendered:?}");
            }
        }
    }

    #[test]
    fn driver_without_argument_is_parse_error() {
        for src in ["driver()", "driver(\"\")"] {
            let err = parse(src, &base()).expect_err("an empty driver must not parse");
            assert!(
                format!("{err:#}").contains("non-empty driver name"),
                "{err:#}"
            );
        }
        let unknown = parse("drivr(x)", &base()).expect_err("unknown function");
        assert!(
            format!("{unknown:#}").contains("package_prefix, driver"),
            "{unknown:#}"
        );
        for bad in [
            "driver(\"unterminated)",
            "driver(\"bad \\q escape\")",
            "driver(\"\\u{zz}\")",
            "driver(\"\\u{d800}\")",
            "driver(\"\\u{1234567}\")",
            "driver(a b)",
            "label(\"unterminated)",
            "label(\"bad \\q escape\")",
        ] {
            assert!(parse(bad, &base()).is_err(), "{bad} should not parse");
        }
    }

    /// Escapes belong to a function's argument only: a quoted bare pattern
    /// tokenizes as it always did, backslash and all.
    #[test]
    fn escapes_scoped_to_function_arguments() {
        // `\` is not an escape outside an argument, so the word ends at the
        // first `"`.
        assert_eq!(p("\"//a\\\""), Matcher::Package(PkgBuf::from("a\\")));
        assert_eq!(
            p("\"//foo/bar\""),
            Matcher::Package(PkgBuf::from("foo/bar"))
        );
        assert_eq!(
            p("\"//foo:bar\" && label(\"x\")"),
            Matcher::And(vec![p("//foo:bar"), Matcher::Label("x".to_string()),])
        );
        // A quoted pattern after a grouping `(` (not a call) has no escapes
        // either.
        assert_eq!(p("(\"//a\\\")"), Matcher::Package(PkgBuf::from("a\\")));
        assert_eq!(p("!(\"//a\\\")"), p("!//a\\"));
        // Inside an argument the same `\"` is an escape, so the literal runs
        // on to the next `"`.
        assert!(parse("package(\"a\\\")", &base()).is_err());
        assert_eq!(
            p("package(\"a\\\\\")"),
            Matcher::Package(PkgBuf::from("a\\"))
        );
    }

    #[test]
    fn moderately_nested_not_still_parses() {
        // The depth cap must not reject ordinary, non-adversarial input.
        let input = format!("{}//a:b", "!".repeat(10));
        assert!(parse(&input, &base()).is_ok());
    }
}
