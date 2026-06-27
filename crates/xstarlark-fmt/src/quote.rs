//! String-literal quoting, ported from the Go reference's `quote.go` /
//! `quoteLiteral` and the `formatComment` helper.

/// Normalize a comment: ensure a single space after `#`, trim trailing
/// whitespace. Input is the raw `# ...` source text.
pub(crate) fn format_comment(raw: &str) -> String {
    let body = raw.strip_prefix('#').unwrap_or(raw);
    let body = if !body.is_empty() && !body.starts_with(' ') {
        format!(" {body}")
    } else {
        body.to_string()
    };
    let body = body.trim_end_matches(|c: char| c.is_whitespace());
    format!("#{body}")
}

/// Quote `s` using the quote string `q` (`"`, `'`, `"""` or `'''`), escaping as
/// little as possible. Port of the Go reference `quoteWith`. The quote
/// character is escaped on every occurrence — for a triple-quoted `q` this
/// yields the same closing-run escaping as the reference.
fn quote_with(s: &str, q: &str) -> String {
    let qchar = q.chars().next().expect("quote string is non-empty");

    let mut buf = String::with_capacity(s.len() + 2 * q.len());
    buf.push_str(q);
    for r in s.chars() {
        match r {
            '\\' => buf.push_str("\\\\"),
            c if c == qchar => {
                buf.push('\\');
                buf.push(c);
            }
            c if is_print(c) => buf.push(c),
            '\x07' => buf.push_str("\\a"),
            '\x08' => buf.push_str("\\b"),
            '\x0c' => buf.push_str("\\f"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            '\x0b' => buf.push_str("\\v"),
            _ => {
                let c = r as u32;
                if c < 0x20 || c == 0x7f {
                    buf.push_str(&format!("\\x{c:02x}"));
                } else if c < 0x10000 {
                    buf.push_str(&format!("\\u{c:04x}"));
                } else {
                    buf.push_str(&format!("\\U{c:08x}"));
                }
            }
        }
    }
    buf.push_str(q);
    buf
}

/// Pick the best quoting for a string literal so as to minimize escaping.
///
/// `value` is the decoded string content; `raw` is the original source token
/// (including quotes). Port of the Go reference `quoteLiteral`: non-printable
/// content or a line-continuation in the raw form is preserved verbatim.
pub(crate) fn quote_literal(value: &str, raw: &str) -> String {
    if value.chars().any(|r| !is_print(r)) {
        return raw.to_string();
    }
    if raw.contains("\\\n") {
        return raw.to_string();
    }

    if value.contains('"') {
        if value.contains('\'') {
            if value.contains("\"\"\"") {
                quote_with(value, "'''")
            } else {
                quote_with(value, "\"\"\"")
            }
        } else {
            quote_with(value, "'")
        }
    } else {
        quote_with(value, "\"")
    }
}

/// Matches Go's `unicode.IsPrint` closely enough for formatting decisions:
/// graphic characters plus ASCII space are printable; everything else
/// (control chars, including `\n`/`\t`, and other whitespace) is not.
fn is_print(r: char) -> bool {
    if r == ' ' {
        return true;
    }
    if r.is_control() {
        return false;
    }
    !r.is_whitespace()
}
