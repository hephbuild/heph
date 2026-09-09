//! The one `${…}` tokenizer.
//!
//! heph grows several places where a string carries a reference to something the
//! author did not write inline — a credential's presentation templates today,
//! driver-option references next. Left alone those become separate parsers with
//! different escapes and overlapping kind names, so that `${env:NAME}` means one
//! thing in one file and another thing two files away.
//!
//! This module is deliberately only the *tokenizer*. It says what the pieces of a
//! string are; it has no opinion about which kinds exist, because that genuinely
//! differs per site — `file`, `field` and `helper` are meaningful only inside a
//! credential presentation, and rejecting them elsewhere is the caller's job. One
//! grammar, one escape, many vocabularies.
//!
//! ```text
//! $$          a literal `$`
//! ${name}     a reference with no kind
//! ${kind:arg} a reference with a kind (split at the FIRST colon)
//! $           anything else after a `$` is a literal `$`
//! ```
//!
//! Substitution is **single-pass** by construction: [`parse`] returns pieces of
//! the *input*, so a replacement value containing `${` is never re-scanned. That
//! matters most where the values are credential material, which an attacker-shaped
//! token could otherwise use to reach a second field.

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Verbatim text, borrowed from the input.
    Text(&'a str),
    /// A `$$` escape, kept as its own piece rather than folded into text.
    ///
    /// Separate because the two consumers disagree about it, and both are right.
    /// A **credential presentation** is a document heph writes, so `$$` is heph's
    /// escape and unescapes to `$`. A **driver option** is somebody else's shell
    /// text — `echo tmp.$$` is the PID idiom — so heph must reproduce it byte for
    /// byte. Folding `$$` into `Text("$")` made the second impossible, and the
    /// symptom was a field whose meaning changed depending on whether a
    /// `${read://…}` happened to appear elsewhere in the same string.
    Escape,
    /// A `${…}` reference.
    Ref(Ref<'a>),
}

/// A parsed `${…}` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref<'a> {
    /// The part before the first `:`, or `None` when there is no colon.
    pub kind: Option<&'a str>,
    /// Everything after the first `:`, or the whole body when there is no colon.
    pub arg: &'a str,
    /// The original text including the `${` and `}`, so a caller that does not
    /// recognize `kind` can emit the reference back unchanged rather than having
    /// to reconstruct (and risk normalizing) it.
    pub raw: &'a str,
}

/// What a `$$` escape unescapes to, for the consumers that own their document.
const DOLLAR: &str = "$";

/// The `${…}` kinds heph **resolves** today.
///
/// One list, in the leaf crate, because two places consult it — the shared spec
/// decoder that *refuses* one in a field that would never substitute it, and the
/// engine walk that *resolves* them — and a kind added to one and not the other
/// fails exactly the way this mechanism exists to prevent: silently, as a
/// literal.
///
/// Deliberately just `read`. `src` and `env` are *designed* but not implemented,
/// and reserving them now would be a break with no benefit: `${src:0:3}` is bash
/// substring expansion on a variable named `src`, and `${env:FOO}` is a template
/// syntax several tools use — both are legal in a `run` today and neither can be
/// misread as something heph resolves, because heph resolves neither. When they
/// ship, reserving them is a stated break in that release, and just as loud as
/// this one. See [`RESERVED_LATER`].
pub const DEFERRED_KINDS: &[&str] = &["read"];

/// Whether a `${kind:arg}` whose kind heph owns is *this* `${…}`.
///
/// The kind name alone is not enough, because the shells and tools whose syntax
/// shares this shape are not going away. `${read:…}` and `${src:…}` name a
/// target, and a target address always begins `//` — so the discriminator is the
/// argument, and it is exact rather than heuristic:
///
/// ```text
/// ${src://infra:v}   heph — a bash arithmetic error, or "" for an unset `src`
/// ${src::3}          bash — the first three characters of $src
/// ${src:0:3}         bash — the same, spelled with an explicit offset
/// ${src:-default}    bash — $src, or "default" when unset
/// ```
///
/// The `:name` and `./` relative forms `parse_addr_with_base` accepts are
/// deliberately **not** claimed: `${src::3}` is the exact text of a real bash
/// idiom, and no diagnostic is worth taking it over. A reference names an
/// absolute address.
///
/// One predicate, every consumer — the spec decoder that refuses one in an
/// ordinary field, the engine walk that collects the edges, and the substitution
/// that fills them in. Two answers to "is this heph's?" is the silent
/// misclassification this module exists to prevent.
pub fn claims(kind: Option<&str>, arg: &str, kinds: &[&str]) -> bool {
    kind.is_some_and(|k| kinds.contains(&k)) && arg.starts_with("//")
}

/// Kinds the design names but does not yet implement.
///
/// Refused — with "not yet", not "unknown" — but **only inside a driver that
/// accepts deferred values**, where an author writing one plainly meant it to
/// resolve. In any other driver they are somebody else's syntax and are left
/// alone.
pub const RESERVED_LATER: &[&str] = &["src"];

/// The first `${kind:…}` in `s` whose kind is in `kinds`.
pub fn first_ref_of<'a>(s: &'a str, kinds: &[&str]) -> Option<Ref<'a>> {
    if !has_ref(s) {
        return None;
    }
    parse(s).ok()?.into_iter().find_map(|p| match p {
        Piece::Ref(r) if claims(r.kind, r.arg, kinds) => Some(r),
        _ => None,
    })
}

/// Whether `s` carries a reference heph owns.
///
/// False for a string with no `${…}` at all, for one whose kinds are all
/// somebody else's, and — deliberately — for one that does not tokenize. An
/// unterminated `${` is a shell program's business until it appears in a field
/// that actually takes a reference; refusing it everywhere would fail a `run`
/// containing `sed 's/${/X/'`, which has nothing to do with this.
pub fn has_deferred_ref(s: &str) -> bool {
    if !has_ref(s) {
        return false;
    }
    let Ok(pieces) = parse(s) else {
        return false;
    };
    pieces.iter().any(|p| match p {
        Piece::Ref(r) => claims(r.kind, r.arg, DEFERRED_KINDS),
        Piece::Text(_) | Piece::Escape => false,
    })
}

/// The first reference heph owns in `s`, if any.
pub fn first_deferred_ref(s: &str) -> Option<Ref<'_>> {
    first_ref_of(s, DEFERRED_KINDS)
}

/// Split `s` into literal and reference pieces.
///
/// Fails only on an unterminated `${`, which is always a typo: silently treating
/// it as literal text would put the reference into whatever the string configures,
/// and the author would find out from the tool rather than from heph.
pub fn parse(s: &str) -> anyhow::Result<Vec<Piece<'_>>> {
    let mut out: Vec<Piece<'_>> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut lit_start = 0usize;
    while i < bytes.len() {
        if bytes.get(i) != Some(&b'$') {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(&b'$') => {
                push_text(&mut out, s.get(lit_start..i));
                out.push(Piece::Escape);
                i += 2;
                lit_start = i;
            }
            Some(&b'{') => {
                let Some(close) = s.get(i + 2..).and_then(|rest| rest.find('}')) else {
                    anyhow::bail!(
                        "unterminated `${{` in template {s:?} — every `${{` needs a closing `}}`, \
                         and a literal `$` is written `$$`"
                    );
                };
                let end = i + 2 + close + 1;
                push_text(&mut out, s.get(lit_start..i));
                let body = s.get(i + 2..end - 1).unwrap_or_default();
                // `${FOO:-${read://a:b}}` closes on the *inner* `}`, so the
                // outer form would tokenize as kind `FOO` with a mangled arg and
                // a stray `}` of text — an unknown kind, reproduced verbatim, so
                // the inner reference never resolves and never complains. Loud,
                // because the silent version is indistinguishable from working.
                if body.contains("${") {
                    anyhow::bail!(
                        "nested `${{` inside `{raw}` — heph does not expand a reference within a \
                         reference, and this would otherwise be passed through untouched. Put the \
                         inner reference in its own field, or write a literal `$` as `$$`",
                        raw = s.get(i..end).unwrap_or_default()
                    );
                }
                let raw = s.get(i..end).unwrap_or_default();
                let (kind, arg) = match body.split_once(':') {
                    Some((k, a)) => (Some(k), a),
                    None => (None, body),
                };
                out.push(Piece::Ref(Ref { kind, arg, raw }));
                i = end;
                lit_start = i;
            }
            // A bare `$` is ordinary text — `$HOME` in a netrc line, a price in a
            // label. Only `$$` and `${` mean anything.
            _ => i += 1,
        }
    }
    push_text(&mut out, s.get(lit_start..));
    Ok(out)
}

fn push_text<'a>(out: &mut Vec<Piece<'a>>, t: Option<&'a str>) {
    if let Some(t) = t
        && !t.is_empty()
    {
        out.push(Piece::Text(t));
    }
}

/// True when `s` contains at least one `${…}` reference.
///
/// Cheap enough to call per string on a parse path: it is a single `memchr`-shaped
/// scan and returns on the first hit, so the overwhelmingly common "no references
/// anywhere" case never allocates.
pub fn has_ref(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = s.get(i..).and_then(|r| r.find('$')) {
        let at = i + pos;
        match bytes.get(at + 1) {
            // `$$` escapes the dollar, so the `{` after it (if any) is literal.
            Some(&b'$') => i = at + 2,
            Some(&b'{') => return true,
            _ => i = at + 1,
        }
    }
    false
}

/// Substitute every reference through `resolve`, concatenating the result.
///
/// `resolve` returns the replacement text for a reference, or an error naming
/// what it did not recognize. Returning an error rather than the original text is
/// the right default for a *closed* vocabulary like a credential presentation; a
/// caller with an open one passes back [`Ref::raw`].
pub fn render(
    s: &str,
    mut resolve: impl FnMut(&Ref<'_>) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let pieces = parse(s)?;
    // The common case is a template with no references at all.
    if pieces.len() == 1
        && let Some(Piece::Text(t)) = pieces.first()
        && std::ptr::eq(*t, s)
    {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    for p in &pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Escape => out.push_str(DOLLAR),
            Piece::Ref(r) => out.push_str(&resolve(r)?),
        }
    }
    Ok(out)
}

/// Replace only the references `want` names, reproducing **everything else byte
/// for byte** — `$$` included.
///
/// This is the driver-option half of the split. A driver option is somebody
/// else's text: `echo tmp.$$` is the shell's PID idiom, `${src:0:3}` is bash
/// substring expansion, `${FOO:-x}` is a default. heph replaces the one
/// construct it owns and touches nothing else, so a field's meaning does not
/// change depending on whether a reference happens to appear elsewhere in it.
pub fn substitute(
    s: &str,
    want: &[&str],
    mut resolve: impl FnMut(&Ref<'_>) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let pieces = parse(s)?;
    if !pieces
        .iter()
        .any(|p| matches!(p, Piece::Ref(r) if r.kind.is_some_and(|k| want.contains(&k))))
    {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    for p in &pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            // Verbatim: not heph's escape here.
            Piece::Escape => out.push_str("$$"),
            Piece::Ref(r) if r.kind.is_some_and(|k| want.contains(&k)) => {
                out.push_str(&resolve(r)?);
            }
            Piece::Ref(r) => out.push_str(r.raw),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(s: &str) -> Vec<(Option<String>, String)> {
        parse(s)
            .expect("parse")
            .into_iter()
            .filter_map(|p| match p {
                Piece::Ref(r) => Some((r.kind.map(str::to_string), r.arg.to_string())),
                Piece::Text(_) | Piece::Escape => None,
            })
            .collect()
    }

    #[test]
    fn a_bare_name_has_no_kind() {
        assert_eq!(refs("${token}"), vec![(None, "token".to_string())]);
    }

    #[test]
    fn the_first_colon_splits_kind_from_arg() {
        // A URL as the arg is the case that makes "first colon" load-bearing:
        // `${file:https://x}` must not split at the scheme's colon.
        assert_eq!(
            refs("${file:https://x}"),
            vec![(Some("file".to_string()), "https://x".to_string())]
        );
    }

    #[test]
    fn dollar_dollar_is_a_literal_dollar_and_does_not_open_a_reference() {
        let out = render("$${token}", |_r| panic!("must not resolve")).expect("render");
        assert_eq!(out, "${token}");
    }

    #[test]
    fn a_bare_dollar_is_literal() {
        assert_eq!(refs("costs $5 and $HOME"), vec![]);
        let out = render("costs $5", |r| {
            anyhow::bail!("a bare `$` opened a reference: {}", r.raw)
        })
        .expect("render");
        assert_eq!(out, "costs $5");
    }

    #[test]
    fn an_unterminated_reference_is_an_error() {
        let err = parse("a ${token").expect_err("must fail");
        assert!(format!("{err:#}").contains("unterminated"), "{err:#}");
    }

    #[test]
    fn substitution_is_single_pass() {
        // The property that matters when the values are secrets: material that
        // happens to contain `${…}` is never re-interpreted.
        let out = render("${a}", |_r| Ok("${b}".to_string())).expect("render");
        assert_eq!(out, "${b}");
    }

    #[test]
    fn render_interleaves_text_and_references() {
        let out = render("machine ${host} login ${user}\n", |r| {
            Ok(match r.arg {
                "host" => "h".to_string(),
                "user" => "u".to_string(),
                other => panic!("unexpected {other}"),
            })
        })
        .expect("render");
        assert_eq!(out, "machine h login u\n");
    }

    /// The two consumers disagree about `$$`, and both are right: a credential
    /// presentation is heph's own document, a driver option is somebody else's
    /// shell text.
    #[test]
    fn an_escape_is_unescaped_by_render_and_reproduced_by_substitute() {
        assert_eq!(
            render("tmp.$$", |_r| panic!("no refs")).expect("render"),
            "tmp.$"
        );
        assert_eq!(
            substitute("tmp.$$", &["read"], |_r| panic!("no refs")).expect("sub"),
            "tmp.$$"
        );
    }

    /// The bug this split exists to remove: a field whose meaning changed
    /// depending on whether a reference happened to be elsewhere in the string.
    #[test]
    fn a_reference_elsewhere_does_not_change_what_the_rest_of_the_field_means() {
        let sub = |s: &str| {
            substitute(s, &["read"], |r| {
                assert_eq!(r.arg, "//a:b");
                Ok("V".to_string())
            })
            .expect("sub")
        };
        assert_eq!(sub("echo tmp.$$"), "echo tmp.$$");
        assert_eq!(sub("echo tmp.$$ ${read://a:b}"), "echo tmp.$$ V");
        // …and a kind heph does not own is left exactly as written.
        assert_eq!(sub("${src:0:3} ${read://a:b}"), "${src:0:3} V");
        assert_eq!(sub("${FOO:-d} ${read://a:b}"), "${FOO:-d} V");
    }

    #[test]
    fn only_the_kind_heph_resolves_counts_as_deferred() {
        assert!(has_deferred_ref("${read://a:b}"));
        assert!(!has_deferred_ref("plain"));
        assert!(
            !has_deferred_ref("$${read://a:b}"),
            "an escaped one is text"
        );
        // Designed, not implemented — flagged only in the shape that names a
        // target, because the other shapes belong to the shell.
        assert!(!has_deferred_ref("${src://a:b}"));
        assert_eq!(
            first_ref_of("${src://a:b}", RESERVED_LATER).map(|r| r.raw),
            Some("${src://a:b}")
        );
        // Bash, every one of them, and heph claims none.
        for bash in [
            "${src:0:3}",
            "${src::3}",
            "${src::-1}",
            "${src:-d}",
            "${src:+x}",
        ] {
            assert!(!has_deferred_ref(bash), "{bash}");
            assert!(first_ref_of(bash, RESERVED_LATER).is_none(), "{bash}");
        }
        // `${env:NAME}` is not heph's syntax at all: it is legal bash
        // (`${env:offset}`) and a template form several tools use, and the value
        // it would have carried composes from a `pass_env` producer plus
        // `${read:}` — see docs/DEFERRED_VALUES.md.
        assert!(!has_deferred_ref("${env:NAME}"));
        assert!(first_ref_of("${env:NAME}", RESERVED_LATER).is_none());
        // Somebody else's, always.
        assert!(!has_deferred_ref("${FOO:-default}"));
        assert!(!has_deferred_ref("${OUT}"));
    }

    /// `${VAR:-default}` is the commonest bash brace form there is, and a
    /// reference inside one closes on the inner `}` — so without this the outer
    /// form tokenizes as an unknown kind, is reproduced verbatim, and the inner
    /// reference silently never resolves.
    #[test]
    fn a_reference_nested_inside_another_brace_form_is_refused() {
        let err = parse("${FOO:-${read://a:b}}").expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("nested"), "{msg}");
        // An inner `$$` is not a reference and does not trip it.
        parse("${FOO:-$$}").expect("an escape is not a nested reference");
        // Neither does a `${` that is merely *after* a complete reference.
        parse("${read://a:b} ${FOO:-d}").expect("two siblings are fine");
    }

    /// An unterminated `${` is a shell program's business. Refusing it in every
    /// string field would fail a `run` containing `sed 's/${/X/'`, which has
    /// nothing to do with deferred values.
    #[test]
    fn a_string_that_does_not_tokenize_is_not_ours() {
        assert!(parse("sed 's/${/X/'").is_err());
        assert!(!has_deferred_ref("sed 's/${/X/'"));
        assert!(first_deferred_ref("sed 's/${/X/'").is_none());
    }

    #[test]
    fn has_ref_agrees_with_parse() {
        for s in [
            "", "plain", "$", "$$", "$${x}", "${x}", "a${x}b", "$$${x}", "$ {x}",
        ] {
            let parsed = parse(s)
                .map(|ps| ps.iter().any(|p| matches!(p, Piece::Ref(_))))
                .unwrap_or(true);
            assert_eq!(has_ref(s), parsed, "disagreed on {s:?}");
        }
    }
}
