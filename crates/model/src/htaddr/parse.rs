use crate::htaddr::addr::Addr;
use crate::htpkg::{PkgBuf, join_rel_checked_pkg};
use anyhow::Context;
use nom::IResult;
use nom::Parser;
use nom::branch::alt;
use nom::bytes::complete::{tag, take_till, take_till1};
use nom::character::complete::char as nchar;
use nom::combinator::{all_consuming, opt};
use nom::error::Error;
use nom::multi::separated_list1;
use nom::sequence::{delimited, preceded};
use serde::Deserialize;
use std::collections::BTreeMap;

impl<'de> Deserialize<'de> for Addr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        // The error quotes the input and says where it went wrong.
        parse_addr(&s).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

type R<'a, T> = IResult<&'a str, T, Error<&'a str>>;

fn pkg(i: &str) -> R<'_, &str> {
    take_till(|c: char| c == ':' || c == ' ').parse(i)
}

fn name(i: &str) -> R<'_, &str> {
    take_till1(|c: char| matches!(c, '@' | ':' | ' ' | '|')).parse(i)
}

fn key(i: &str) -> R<'_, &str> {
    take_till1(|c: char| matches!(c, '=' | ',' | '@' | ' ' | '|' | '"')).parse(i)
}

fn bare_value(i: &str) -> R<'_, &str> {
    take_till1(|c: char| matches!(c, ',' | ' ' | '|' | '"')).parse(i)
}

fn quoted_value(i: &str) -> R<'_, &str> {
    delimited(nchar('"'), take_till(|c| c == '"'), nchar('"')).parse(i)
}

fn value(i: &str) -> R<'_, &str> {
    alt((quoted_value, bare_value)).parse(i)
}

fn arg(i: &str) -> R<'_, (&str, &str)> {
    let (i, k) = key(i)?;
    let (i, v) = opt(preceded(nchar('='), value)).parse(i)?;
    Ok((i, (k, v.unwrap_or(""))))
}

fn args(i: &str) -> R<'_, Vec<(&str, &str)>> {
    separated_list1(nchar(','), arg).parse(i)
}

#[expect(
    clippy::type_complexity,
    reason = "tuple return mirrors grammar productions; refactoring into a struct would obscure the parser"
)]
fn addr_parser(i: &str) -> R<'_, (&str, &str, Vec<(&str, &str)>)> {
    let (i, _) = tag("//").parse(i)?;
    let (i, p) = pkg(i)?;
    let (i, _) = nchar(':').parse(i)?;
    let (i, n) = name(i)?;
    let (i, a) = opt(preceded(nchar('@'), args)).parse(i)?;
    Ok((i, (p, n, a.unwrap_or_default())))
}

fn resolve_relative_pkg(base: &PkgBuf, rel: &str) -> anyhow::Result<String> {
    join_rel_checked_pkg(base.as_str(), rel)
}

pub fn parse_addr_with_base(input: &str, base: &PkgBuf) -> anyhow::Result<Addr> {
    if input.starts_with("//") {
        return parse_addr(input);
    }

    // ":name" or ":name@args"
    if input.starts_with(':') {
        return parse_addr(&format!("//{}{}", base, input));
    }

    // "./path:name" or "../path:name", optionally followed by "@args".
    //
    // An explicit ':name' is required. We deliberately do NOT derive a target
    // name from the last path component (no "./sub" -> "//base/sub:sub"
    // shorthand): a bare "./somefile.txt" is indistinguishable from a filesystem
    // path, so it must not silently resolve into a target. Callers that want the
    // path treated as a file handle that themselves.
    if input.starts_with("./") || input.starts_with("../") {
        let Some(colon_pos) = input.find(':') else {
            return Err(anyhow::anyhow!(
                "invalid address '{input}': a relative path reference must name a target, e.g. '{input}:name'"
            ));
        };
        // colon_pos comes from find(':') — an ASCII single-byte char — so both
        // slices land on valid UTF-8 char boundaries.
        #[expect(
            clippy::string_slice,
            reason = "index derived from ASCII char position, always a valid boundary"
        )]
        let path_part = &input[..colon_pos];
        #[expect(
            clippy::string_slice,
            reason = "index derived from ASCII char position, always a valid boundary"
        )]
        let rest = &input[colon_pos..]; // ":name" or ":name@args"
        let pkg = resolve_relative_pkg(base, path_part)?;
        return parse_addr(&format!("//{pkg}{rest}"));
    }

    // A bare "name" (no leading ':', './', '../', or '//') is too ambiguous to
    // accept — it reads like a plain identifier yet would silently resolve into
    // the current package. Require an explicit ':' so relative references are
    // unmistakable.
    Err(anyhow::anyhow!(
        "invalid address '{input}': relative references must start with ':', './', '../', or '//'"
    ))
}

/// The characters a target name may not contain: the address parser's
/// separators (`@`, `:`, `|`, a space), and `/`, since a name is a directory
/// component of the target's cache entry (`<home>/cache/<pkg>/__target_<name>`)
/// and a `/` would nest it inside another target's, or `..` segments climb out.
const NAME_FORBIDDEN: &str = "a name may not contain `@`, `:`, `|`, `/` or a space";

/// Refuse a target name outside the grammar: empty, or holding one of
/// [`NAME_FORBIDDEN`]'s characters. The message names the character and the
/// rule. Every other name, unicode included, is valid.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("target name cannot be empty");
    }
    if let Some(c) = name
        .chars()
        .find(|c| matches!(c, '@' | ':' | '|' | '/' | ' '))
    {
        let what = if c == ' ' {
            "a space".to_owned()
        } else {
            format!("`{c}`")
        };
        anyhow::bail!("target name `{name}` contains {what}; {NAME_FORBIDDEN}");
    }
    Ok(())
}

/// An address the grammar refuses, pointing at where (GRAPH-03):
/// "`//pkg:abc:x` is not a valid address: unexpected `:` at column 10".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddrParseError {
    /// The input, as given.
    pub input: String,
    /// The 1-based column, in characters, the parser stopped at.
    pub column: usize,
    reason: String,
}

impl std::fmt::Display for AddrParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is not a valid address: {}",
            self.input.escape_debug(),
            self.reason
        )
    }
}

impl std::error::Error for AddrParseError {}

impl AddrParseError {
    /// The parser stopped at `rest`, a suffix of `input`, failing `code`.
    fn at(input: &str, rest: &str, code: Option<nom::error::ErrorKind>) -> Self {
        let offset = input.len().saturating_sub(rest.len());
        let column = input.get(..offset).map_or(0, |s| s.chars().count()) + 1;
        let reason = match rest.chars().next() {
            Some(c) if c.is_whitespace() => {
                let place = if rest.trim().is_empty() {
                    "trailing "
                } else if offset == 0 {
                    "leading "
                } else {
                    ""
                };
                format!("unexpected {place}whitespace at column {column}")
            }
            _ if code == Some(nom::error::ErrorKind::Tag) => {
                "an address starts with `//`, as in `//pkg:name`".to_owned()
            }
            Some(c) => format!("unexpected `{}` at column {column}", c.escape_debug()),
            None => match code {
                Some(nom::error::ErrorKind::Char) => {
                    format!("expected `:` and a target name at column {column}")
                }
                Some(nom::error::ErrorKind::TakeTill1) => {
                    format!("expected a target name at column {column}")
                }
                _ => format!("unexpected end at column {column}"),
            },
        };
        Self {
            input: input.to_owned(),
            column,
            reason,
        }
    }
}

pub fn parse_addr(input: &str) -> anyhow::Result<Addr> {
    let (_, (p, n, kvs)) = all_consuming(addr_parser)
        .parse(input)
        .map_err(|e| match e {
            nom::Err::Error(e) | nom::Err::Failure(e) => {
                AddrParseError::at(input, e.input, Some(e.code))
            }
            nom::Err::Incomplete(_) => AddrParseError::at(input, "", None),
        })?;

    // The absolute `//pkg:name` form has no base to resolve against, but a
    // `..` segment in `pkg` must still be bound to the workspace root — same
    // rule `resolve_relative_pkg` enforces for relative references, and the
    // same mechanism `fs.file`/`fs.glob` use (`join_rel_checked`). Without
    // this, `//../../etc:passwd` parses cleanly and escapes into filesystem
    // joins downstream (BUILD discovery, cache dir, sandbox dir).
    let package =
        join_rel_checked_pkg("", p).with_context(|| format!("invalid address {input:?}"))?;

    let mut args = BTreeMap::new();
    for (k, v) in kvs {
        args.insert(k.to_string(), v.to_string());
    }

    Ok(Addr::new(PkgBuf::from(package), n.to_string(), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_taddr_simple() {
        let input = "//pkg:name";
        let res = parse_addr(input).unwrap();
        assert_eq!(res.package, "pkg");
        assert_eq!(res.name, "name");
        assert!(res.args.is_empty());
    }

    #[test]
    fn test_parse_taddr_args() {
        let input = "//pkg:name@a=b,c=\"d e\"";
        let res = parse_addr(input).unwrap();
        assert_eq!(res.package, "pkg");
        assert_eq!(res.name, "name");
        assert_eq!(res.args.get("a").unwrap(), "b");
        assert_eq!(res.args.get("c").unwrap(), "d e");
    }

    #[test]
    fn test_parse_taddr_no_value() {
        let input = "//pkg:name@a";
        let res = parse_addr(input).unwrap();
        assert_eq!(res.args.get("a").unwrap(), "");
    }

    #[test]
    fn test_parse_addr_with_base_absolute() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("//other:name", &base).unwrap();
        assert_eq!(res.package, "other");
        assert_eq!(res.name, "name");
    }

    #[test]
    fn test_parse_addr_with_base_colon_name() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base(":mytarget", &base).unwrap();
        assert_eq!(res.package, "base/pkg");
        assert_eq!(res.name, "mytarget");
    }

    #[test]
    fn test_parse_addr_with_base_bare_name_rejected() {
        // A bare identifier is ambiguous and must be written as `:mytarget`.
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("mytarget", &base);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_addr_with_base_bare_name_args_rejected() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("mytarget@k=v", &base);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_addr_with_base_colon_name_args() {
        // The explicit colon form still carries args through.
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base(":mytarget@k=v", &base).unwrap();
        assert_eq!(res.package, "base/pkg");
        assert_eq!(res.name, "mytarget");
        assert_eq!(res.args.get("k").unwrap(), "v");
    }

    #[test]
    fn test_parse_addr_with_base_dot_slash_requires_name() {
        // No derive-name shorthand: a bare "./sub" is ambiguous with a path and
        // must name a target explicitly.
        let base = PkgBuf::from("a/b");
        assert!(parse_addr_with_base("./sub", &base).is_err());
    }

    #[test]
    fn test_parse_addr_with_base_dot_slash_explicit_name() {
        let base = PkgBuf::from("a/b");
        let res = parse_addr_with_base("./sub:other", &base).unwrap();
        assert_eq!(res.package, "a/b/sub");
        assert_eq!(res.name, "other");
    }

    #[test]
    fn test_parse_addr_with_base_dot_dot_slash_requires_name() {
        let base = PkgBuf::from("a/b/c");
        assert!(parse_addr_with_base("../sibling", &base).is_err());
    }

    #[test]
    fn test_parse_addr_with_base_dot_dot_slash_explicit_name_args() {
        let base = PkgBuf::from("a/b/c");
        let res = parse_addr_with_base("../sibling:sibling@k=v", &base).unwrap();
        assert_eq!(res.package, "a/b/sibling");
        assert_eq!(res.name, "sibling");
        assert_eq!(res.args.get("k").unwrap(), "v");
    }

    #[test]
    fn test_parse_addr_with_base_escapes_root_fails() {
        let base = PkgBuf::from("a");
        let res = parse_addr_with_base("../../escape:x", &base);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_taddr_empty_package() {
        let res = parse_addr("//:build_lib").unwrap();
        assert_eq!(res.package.as_str(), "");
        assert_eq!(res.name, "build_lib");
        assert!(res.args.is_empty());
    }

    #[test]
    fn test_parse_taddr_empty_package_with_args() {
        let res = parse_addr("//:build_lib@goos=linux,goarch=amd64").unwrap();
        assert_eq!(res.package.as_str(), "");
        assert_eq!(res.name, "build_lib");
        assert_eq!(res.args.get("goos").unwrap(), "linux");
        assert_eq!(res.args.get("goarch").unwrap(), "amd64");
    }

    #[test]
    fn test_parse_addr_absolute_dotdot_escapes_root_fails() {
        // //../../etc:passwd must not parse into a package that walks above
        // the workspace root — it used to (path-traversal into BUILD
        // discovery / cache / sandbox dirs downstream).
        let res = parse_addr("//../../etc:passwd");
        assert!(res.is_err(), "expected error, got {res:?}");
    }

    #[test]
    fn test_parse_addr_absolute_dotdot_within_bounds_normalizes() {
        // A `..` that stays within the package tree is fine and normalizes
        // like `join_rel_checked` does elsewhere.
        let res = parse_addr("//a/b/../c:name").unwrap();
        assert_eq!(res.package.as_str(), "a/c");
    }

    #[test]
    fn test_parse_addr_with_base_absolute_dotdot_escapes_root_fails() {
        // The absolute form bypasses `resolve_relative_pkg`'s guard by going
        // straight through `parse_addr` — must still be rejected.
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("//../../etc:passwd", &base);
        assert!(res.is_err(), "expected error, got {res:?}");
    }

    #[test]
    fn test_parse_taddr_invalid() {
        let cases = vec![
            "pkg:name",           // Missing //
            "//pkg name",         // Missing :
            "//pkg:",             // Empty name ident
            "//pkg:name@a=\"val", // Unclosed quote
            "//pkg:name@a=b c",   // Unexpected space/trailing content
            "//pkg:name:",        // Unexpected space/trailing content
        ];

        for case in cases {
            let res = parse_addr(case);
            assert!(res.is_err(), "Expected error for case: {}", case);
        }
    }

    fn outcome(r: anyhow::Result<Addr>) -> String {
        match r {
            Ok(a) => format!("ok {a} pkg={:?}", a.package.as_str()),
            Err(e) => format!("err {e:#}"),
        }
    }

    /// Every outcome, error text included, pinned: what `parse_addr` accepts,
    /// what it normalizes the package to, and the exact message it rejects
    /// with. Near-miss names, empty input, non-ASCII and control bytes, and
    /// `.`/`..` segments in every position.
    #[test]
    fn parse_addr_golden() {
        let cases: &[(&str, &str)] = &[
            (
                "",
                "err `` is not a valid address: an address starts with `//`, as in `//pkg:name`",
            ),
            (
                "/",
                "err `/` is not a valid address: an address starts with `//`, as in `//pkg:name`",
            ),
            (
                "//",
                "err `//` is not a valid address: expected `:` and a target name at column 3",
            ),
            (
                "//:",
                "err `//:` is not a valid address: expected a target name at column 4",
            ),
            ("//:a", r#"ok //:a pkg="""#),
            ("//a:b", r#"ok //a:b pkg="a""#),
            ("//a/:b", r#"ok //a:b pkg="a""#),
            ("//a//b:c", r#"ok //a/b:c pkg="a/b""#),
            ("///a:b", r#"ok //a:b pkg="a""#),
            ("//a/./b:c", r#"ok //a/b:c pkg="a/b""#),
            ("//./a:b", r#"ok //a:b pkg="a""#),
            ("//.:a", r#"ok //:a pkg="""#),
            (
                "//..:a",
                r#"err invalid address "//..:a": path '..' escapes workspace root"#,
            ),
            ("//a/..:b", r#"ok //:b pkg="""#),
            (
                "//a/../..:b",
                r#"err invalid address "//a/../..:b": path 'a/../..' escapes workspace root"#,
            ),
            ("//a/b/..:c", r#"ok //a:c pkg="a""#),
            ("//a/.b:c", r#"ok //a/.b:c pkg="a/.b""#),
            ("//a/..b:c", r#"ok //a/..b:c pkg="a/..b""#),
            ("//a/b.:c", r#"ok //a/b.:c pkg="a/b.""#),
            ("//a/...:b", r#"ok //a/...:b pkg="a/...""#),
            ("//*.go/:x", r#"ok //*.go:x pkg="*.go""#),
            ("//a/:b@k=v", r#"ok //a:b@k=v pkg="a""#),
            ("//é/ü:ñ", r#"ok //é/ü:ñ pkg="é/ü""#),
            (
                "//a:b ",
                "err `//a:b ` is not a valid address: unexpected trailing whitespace at column 6",
            ),
            (
                " //a:b",
                "err ` //a:b` is not a valid address: unexpected leading whitespace at column 1",
            ),
            (
                "//a b:c",
                "err `//a b:c` is not a valid address: unexpected whitespace at column 4",
            ),
            (
                "//a:b|c",
                "err `//a:b|c` is not a valid address: unexpected `|` at column 6",
            ),
            (
                "//a:b@",
                "err `//a:b@` is not a valid address: unexpected `@` at column 6",
            ),
            (
                "//a:b@=v",
                "err `//a:b@=v` is not a valid address: unexpected `@` at column 6",
            ),
            (
                "//a:b:c",
                "err `//a:b:c` is not a valid address: unexpected `:` at column 6",
            ),
            (
                "//a::b",
                "err `//a::b` is not a valid address: unexpected `:` at column 5",
            ),
            (
                "//a\u{7}:b:c",
                "err `//a\\u{7}:b:c` is not a valid address: unexpected `:` at column 7",
            ),
            ("//pkg:name", r#"ok //pkg:name pkg="pkg""#),
            ("//pkg:nam", r#"ok //pkg:nam pkg="pkg""#),
            ("//pkg:name2", r#"ok //pkg:name2 pkg="pkg""#),
            ("//pkg/:name", r#"ok //pkg:name pkg="pkg""#),
            ("//pkg/sub:name", r#"ok //pkg/sub:name pkg="pkg/sub""#),
            ("//a\u{0}b:c", "ok //a\u{0}b:c pkg=\"a\\0b\""),
            ("//a/\u{7f}:c", "ok //a/\u{7f}:c pkg=\"a/\\u{7f}\""),
        ];
        for (input, want) in cases {
            assert_eq!(&outcome(parse_addr(input)), want, "input {input:?}");
        }
    }

    /// The typed error of `input`.
    fn refused(input: &str) -> AddrParseError {
        let e = parse_addr(input).expect_err("refused");
        e.downcast::<AddrParseError>().expect("an AddrParseError")
    }

    /// GRAPH-03: nom's `Parsing Error: Error { input: ":x", code: Eof }`
    /// named neither the character nor where it was.
    #[test]
    fn malformed_addr_double_colon_points_at_column() {
        let e = refused("//pkg:abc:x");
        assert_eq!(
            e.to_string(),
            "`//pkg:abc:x` is not a valid address: unexpected `:` at column 10"
        );
        assert_eq!(e.column, 10);
    }

    /// A copy-pasted trailing space says it is whitespace, and where.
    #[test]
    fn malformed_addr_trailing_space() {
        let e = refused("//pkg:abc ");
        assert_eq!(
            e.to_string(),
            "`//pkg:abc ` is not a valid address: unexpected trailing whitespace at column 10"
        );
        assert_eq!(
            refused("//pkg:ab c").to_string(),
            "`//pkg:ab c` is not a valid address: unexpected whitespace at column 9"
        );
    }

    #[test]
    fn malformed_addr_pipe_out() {
        let e = refused("//p:t|out");
        assert_eq!(
            e.to_string(),
            "`//p:t|out` is not a valid address: unexpected `|` at column 6"
        );
    }

    /// The column counts characters, not bytes: `é` is one column.
    #[test]
    fn malformed_addr_unicode_column_in_chars() {
        let e = refused("//pké:né:x");
        assert_eq!(e.column, 9);
        assert_eq!(
            e.to_string(),
            "`//pké:né:x` is not a valid address: unexpected `:` at column 9"
        );
    }

    #[test]
    fn parse_addr_with_base_golden() {
        let base = PkgBuf::from("x/y");
        let bare = "relative references must start with ':', './', '../', or '//'";
        let cases: &[(&str, String)] = &[
            ("", format!("err invalid address '': {bare}")),
            (
                ":",
                "err `//x/y:` is not a valid address: expected a target name at column 7"
                    .to_string(),
            ),
            (":a", r#"ok //x/y:a pkg="x/y""#.to_string()),
            ("a", format!("err invalid address 'a': {bare}")),
            (
                "./a",
                "err invalid address './a': a relative path reference must name a target, e.g. './a:name'"
                    .to_string(),
            ),
            ("./a:b", r#"ok //x/y/a:b pkg="x/y/a""#.to_string()),
            ("../a:b", r#"ok //x/a:b pkg="x/a""#.to_string()),
            ("../../a:b", r#"ok //a:b pkg="a""#.to_string()),
            (
                "../../../a:b",
                "err path 'x/y/../../../a' escapes workspace root".to_string(),
            ),
            (".:a", format!("err invalid address '.:a': {bare}")),
            ("..:a", format!("err invalid address '..:a': {bare}")),
            ("./:a", r#"ok //x/y:a pkg="x/y""#.to_string()),
            (".//a:b", r#"ok //x/y/a:b pkg="x/y/a""#.to_string()),
            ("./a/:b", r#"ok //x/y/a:b pkg="x/y/a""#.to_string()),
            ("./a//b:c", r#"ok //x/y/a/b:c pkg="x/y/a/b""#.to_string()),
            ("./a/./b:c", r#"ok //x/y/a/b:c pkg="x/y/a/b""#.to_string()),
            ("./é:ü", r#"ok //x/y/é:ü pkg="x/y/é""#.to_string()),
            ("./*.go/:x", r#"ok //x/y/*.go:x pkg="x/y/*.go""#.to_string()),
            (":a@k=v", r#"ok //x/y:a@k=v pkg="x/y""#.to_string()),
            ("./a:b@k=v", r#"ok //x/y/a:b@k=v pkg="x/y/a""#.to_string()),
            ("../..:a", r#"ok //:a pkg="""#.to_string()),
            ("./..:a", r#"ok //x:a pkg="x""#.to_string()),
        ];
        for (input, want) in cases {
            assert_eq!(
                &outcome(parse_addr_with_base(input, &base)),
                want,
                "input {input:?}"
            );
        }
    }
}
