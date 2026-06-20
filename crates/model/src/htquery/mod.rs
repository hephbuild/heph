//! Query language for selecting targets.
//!
//! Parses an expression like `//some/... && label(foo)` into a [`Matcher`].
//! Every [`Matcher`] variant is reachable:
//!
//! - bare patterns — `//pkg` ([`Matcher::Package`]), `//pkg/...`
//!   ([`Matcher::PackagePrefix`]), `//pkg:name` ([`Matcher::Addr`]); relative
//!   forms (`./x`, `../x`, `.`, `..`) resolve against the base package.
//! - functions — `label(x)`, `tree_output(pkg)`, plus the explicit
//!   `addr(x)`, `package(x)`, `package_prefix(x)` forms.
//! - operators — `&&` ([`Matcher::And`]), `||` ([`Matcher::Or`]),
//!   `!` ([`Matcher::Not`]), and `( … )` grouping.
//!
//! Precedence is `!` > `&&` > `||`; same-precedence chains keep source order so
//! the engine's short-circuit evaluation runs left-to-right and bails early.
//!
//! See [`parse`] for the entry point.

use crate::htaddr::parse_addr_with_base;
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
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
        Matcher::Addr(a) => out.push_str(&a.format()),
        Matcher::Package(p) => {
            out.push_str("//");
            out.push_str(p.as_str());
        }
        Matcher::PackagePrefix(p) => {
            out.push_str("//");
            if p.as_str().is_empty() {
                out.push_str("...");
            } else {
                out.push_str(p.as_str());
                out.push_str("/...");
            }
        }
        Matcher::Label(l) => {
            out.push_str("label(");
            out.push_str(l);
            out.push(')');
        }
        Matcher::TreeOutputTo(p) => {
            out.push_str("tree_output(");
            out.push_str(p.as_str());
            out.push(')');
        }
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
        }
    }
}

/// Split a query into tokens. Words are runs of any character except
/// whitespace and the operator metacharacters `( ) & | !`; `"…"` wraps a word
/// that needs those characters (e.g. a label with a space).
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
                    if c.is_whitespace() || matches!(c, '(' | ')' | '&' | '|' | '!' | '"') {
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

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
    base: &'a PkgBuf,
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
    fn parse_not(&mut self) -> Result<Matcher> {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.bump();
            return Ok(Matcher::Not(Box::new(self.parse_not()?)));
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
                    let arg = match self.bump() {
                        Some(Tok::Word(a)) => a.clone(),
                        Some(other) => {
                            bail!("expected argument to `{w}()`, found `{}`", other.describe())
                        }
                        None => bail!("missing argument to `{w}()`"),
                    };
                    match self.bump() {
                        Some(Tok::RParen) => {}
                        Some(other) => {
                            bail!(
                                "expected `)` after `{w}({arg})`, found `{}`",
                                other.describe()
                            )
                        }
                        None => bail!("unclosed `(` in `{w}(`"),
                    }
                    self.func_to_matcher(w, &arg)
                } else {
                    self.pattern_to_matcher(w)
                }
            }
            Some(other) => bail!("unexpected `{}` in query", other.describe()),
            None => bail!("unexpected end of query, expected a pattern"),
        }
    }

    fn func_to_matcher(&self, name: &str, arg: &str) -> Result<Matcher> {
        match name {
            "label" => Ok(Matcher::Label(arg.to_string())),
            "tree_output" | "tree_output_to" => Ok(Matcher::TreeOutputTo(to_pkg(arg))),
            "addr" => Ok(Matcher::Addr(
                parse_addr_with_base(arg, self.base)
                    .with_context(|| format!("parsing addr({arg})"))?,
            )),
            "package" | "pkg" => Ok(Matcher::Package(to_pkg(arg))),
            "package_prefix" => Ok(Matcher::PackagePrefix(to_pkg(arg))),
            other => bail!(
                "unknown query function `{other}` (expected one of: label, tree_output, addr, package, package_prefix)"
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
/// are accepted, and drop any trailing `/`.
fn to_pkg(arg: &str) -> PkgBuf {
    let arg = arg.strip_prefix("//").unwrap_or(arg);
    PkgBuf::from(arg.trim_end_matches('/'))
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
        assert_eq!(
            p("label(//tag:release)"),
            Matcher::Label("//tag:release".to_string())
        );
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
    fn quoted_label_with_spaces() {
        assert_eq!(
            p("label(\"my label\")"),
            Matcher::Label("my label".to_string())
        );
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
    }

    #[test]
    fn err_missing_func_arg() {
        assert!(parse("label()", &base()).is_err());
    }
}
