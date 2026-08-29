//! The label grammar.
//!
//! A label is a free-form tag a target carries (`labels = ["lint", "go-test"]`)
//! and the only thing `label(x)` in the query language selects on. Its
//! alphabet is deliberately narrow — ASCII letters, digits, `-` and `_` — so
//! that a label is never confused with the syntax that surrounds it:
//!
//! - `heph run <LABEL> <PACKAGE>` takes a bare label positionally, with no
//!   delimiters. Without a grammar, `heph run 'lint && !go-lint' //...` is a
//!   perfectly well-formed request for the label *literally spelled*
//!   `lint && !go-lint`, which no target carries — so it selects nothing and
//!   exits 0, reading exactly like a build that passed.
//! - `//`, `:` and `@` are address syntax; a label shaped like `//tag:release`
//!   invites the reader to believe labels and addresses share a namespace.
//! - `&& || ! ( ) "` are query operators, and whitespace is the query
//!   tokenizer's separator, so such a label is unselectable from `-e`.
//!
//! Rejecting these at the two ends — where a label is *declared* and where one
//! is *selected* — keeps the set of labels a query can name equal to the set a
//! target can carry.

/// Characters a label may be built from: `A-Z`, `a-z`, `0-9`, `-`, `_`.
fn is_label_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Whether `s` is a well-formed label.
pub fn is_valid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_label_char)
}

/// Validate a label, with an error naming the first offending character.
///
/// Callers that took the label from a positional CLI argument should surface
/// [`looks_like_query_expr`] as a hint alongside this error.
pub fn validate(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        anyhow::bail!("empty label: a label must contain at least one character");
    }
    if let Some(bad) = s.chars().find(|c| !is_label_char(*c)) {
        anyhow::bail!(
            "invalid label {s:?}: {} is not allowed — a label may contain only \
             ASCII letters, digits, `-` and `_`",
            describe_char(bad),
        );
    }
    Ok(())
}

/// Render an offending character for an error message. Whitespace and control
/// characters have no useful printed form, so name them instead of emitting
/// them raw into the terminal.
fn describe_char(c: char) -> String {
    match c {
        ' ' => "a space".to_string(),
        '\t' => "a tab".to_string(),
        c if c.is_whitespace() || c.is_control() => format!("U+{:04X}", c as u32),
        c => format!("`{c}`"),
    }
}

/// Whether an invalid label reads like someone typed a query expression where a
/// bare label was expected (`heph run 'lint && !go-lint' //...`). Used only to
/// add a "did you mean `-e`?" hint to the error — never to accept the input.
pub fn looks_like_query_expr(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '&' | '|' | '!' | '(' | ')') || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_alphabet() {
        for s in ["lint", "go-lint", "go_lint", "Lint2", "a", "0", "-", "_"] {
            assert!(is_valid(s), "{s:?} should be valid");
            assert!(validate(s).is_ok(), "{s:?} should validate");
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_valid(""));
        let err = validate("").unwrap_err().to_string();
        assert!(err.contains("empty label"), "{err}");
    }

    #[test]
    fn rejects_query_operators_and_whitespace() {
        for s in [
            "lint && !go-lint",
            "lint&&go",
            "a b",
            "a|b",
            "(a)",
            "!a",
            "a\tb",
        ] {
            assert!(!is_valid(s), "{s:?} should be invalid");
            assert!(validate(s).is_err(), "{s:?} should fail validation");
        }
    }

    #[test]
    fn rejects_address_syntax() {
        // Labels and addresses are separate namespaces; an addr-shaped label
        // suggests otherwise.
        for s in ["//tag:release", "tag:release", "//team/foo", "name@k=v"] {
            assert!(!is_valid(s), "{s:?} should be invalid");
        }
    }

    #[test]
    fn rejects_non_ascii() {
        assert!(!is_valid("café"));
        assert!(!is_valid("日本語"));
    }

    #[test]
    fn error_names_the_offending_character() {
        let err = validate("go lint").unwrap_err().to_string();
        assert!(err.contains("a space"), "{err}");
        let err = validate("//tag:release").unwrap_err().to_string();
        assert!(err.contains("`/`"), "{err}");
    }

    #[test]
    fn query_expr_hint_fires_on_operators_not_on_plain_typos() {
        assert!(looks_like_query_expr("lint && !go-lint"));
        assert!(looks_like_query_expr("a b"));
        assert!(!looks_like_query_expr("//tag:release"));
        assert!(!looks_like_query_expr("café"));
    }
}
