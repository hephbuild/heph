//! "Did you mean": the one near-miss helper every error that names an
//! unknown thing uses (a target, an option, a driver, a config key, a
//! subcommand).
//!
//! It fails closed: no suggestion is better than a wrong one.
//! - A candidate is suggested when it differs from the input by case or
//!   Unicode normalization only (`Foo` / `foo`, NFD / NFC `café`), or by a
//!   Damerau-Levenshtein distance of at most `max(1, len / 4)` over the
//!   case-folded, NFC-normalized names. An input of 1–2 chars gets only the
//!   first kind, never an edit-distance one.
//! - Only the best rank is kept: one clear winner gives one name, candidates
//!   that tie are all listed (at most [`SUGGESTIONS_MAX`]), in name order.
//! - An exact match is not a suggestion: the helper returns nothing.
//! - Duplicate candidates count once.
//! - **Caps:** at most [`CANDIDATES_CAP`] candidates, and a name over
//!   [`NAME_CAP`] chars is skipped (an input over it gets nothing). Over the
//!   candidate cap there is no suggestion, only the count. A length
//!   prefilter runs before any distance is computed, so the cost is bounded
//!   by the candidates whose length could match.
//!
//! The names it returns are the candidates' own text; a renderer escapes
//! control bytes ([`did_you_mean`], [`listed`] do).

use crate::diagnostic::{Hint, HintKind, escape_control};
use std::borrow::Cow;
use std::fmt::Write as _;
use unicode_normalization::UnicodeNormalization as _;

/// The most names one suggestion offers.
pub const SUGGESTIONS_MAX: usize = 3;
/// The most candidates a suggestion is computed over; over it, the count only.
pub const CANDIDATES_CAP: usize = 5_000;
/// The longest name, in chars, compared at all.
pub const NAME_CAP: usize = 128;

/// What [`suggestions`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suggestions<'a> {
    /// The nearest candidates, best first; empty for none.
    pub names: Vec<&'a str>,
    /// The candidate count when it was over [`CANDIDATES_CAP`]: nothing was
    /// compared.
    pub over_cap: Option<usize>,
}

impl Suggestions<'_> {
    /// As hints: one [`HintKind::DidYouMean`] per name, or the count of
    /// candidates when there were too many to compare.
    pub fn hints(&self) -> Vec<Hint> {
        match self.over_cap {
            Some(n) => vec![Hint::new(
                HintKind::Other,
                format!("{n} candidates, too many to suggest from"),
            )],
            None => did_you_mean(&self.names),
        }
    }
}

/// The nearest of `candidates` to `input`, best first, at most
/// [`SUGGESTIONS_MAX`]. See the module docs.
pub fn suggest<'a>(input: &str, candidates: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    suggestions(input, candidates).names
}

/// [`suggest`], also saying whether the candidates were over the cap.
pub fn suggestions<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Suggestions<'a> {
    rank(input, candidates, &mut 0)
}

/// [`suggestions`], counting each distance it computes into `distances`.
fn rank<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    distances: &mut usize,
) -> Suggestions<'a> {
    let mut kept: Vec<&'a str> = Vec::new();
    let mut count = 0usize;
    for c in candidates {
        count += 1;
        if count <= CANDIDATES_CAP {
            kept.push(c);
        }
    }
    if count > CANDIDATES_CAP {
        return Suggestions {
            names: Vec::new(),
            over_cap: Some(count),
        };
    }
    if input.is_empty() || over_name_cap(input) || kept.contains(&input) {
        return Suggestions::default();
    }
    kept.sort_unstable();
    kept.dedup();

    let key = fold(input);
    let len = key.chars().count();
    let short = len <= 2;
    let max = (len / 4).max(1);
    let mut best: Option<usize> = None;
    let mut names: Vec<&'a str> = Vec::new();
    for c in kept {
        if over_name_cap(c) {
            continue;
        }
        // The prefilter: a candidate whose length differs by more than the
        // allowed distance can match neither way. For two ASCII names the
        // raw lengths decide it before anything is allocated.
        if input.is_ascii() && c.is_ascii() && c.len().abs_diff(input.len()) > max {
            continue;
        }
        let folded = fold(c);
        let rank = if folded == key {
            0
        } else if short || folded.chars().count().abs_diff(len) > max {
            continue;
        } else {
            *distances += 1;
            let d = strsim::damerau_levenshtein(&folded, &key);
            if d > max {
                continue;
            }
            d
        };
        match best {
            Some(b) if rank > b => continue,
            Some(b) if rank == b => names.push(c),
            _ => {
                best = Some(rank);
                names.clear();
                names.push(c);
            }
        }
    }
    names.truncate(SUGGESTIONS_MAX);
    Suggestions {
        names,
        over_cap: None,
    }
}

fn over_name_cap(s: &str) -> bool {
    // Bytes bound chars from above: only a long name is counted.
    s.len() > NAME_CAP && s.chars().count() > NAME_CAP
}

/// `s` NFC-normalized and lowercased: the form names are compared in.
fn fold(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            return Cow::Owned(s.to_ascii_lowercase());
        }
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.nfc().flat_map(char::to_lowercase).collect())
}

/// One [`HintKind::DidYouMean`] per name, its control bytes escaped.
pub fn did_you_mean(names: &[&str]) -> Vec<Hint> {
    names
        .iter()
        .map(|n| Hint::new(HintKind::DidYouMean, escape_control(n)))
        .collect()
}

/// The names as inline prose for a message with no hint rows:
/// "did you mean `a`?", "did you mean `a` or `b`?". `None` for no names.
pub fn did_you_mean_text(names: &[&str]) -> Option<String> {
    let (last, rest) = names.split_last()?;
    let mut out = String::from("did you mean ");
    for n in rest {
        // Writing to a `String` cannot fail.
        write!(out, "`{}`, ", escape_control(n)).unwrap_or_default();
    }
    if !rest.is_empty() {
        // "`a`, `b`, " → "`a`, `b` or "
        out.truncate(out.len() - 2);
        out.push_str(" or ");
    }
    write!(out, "`{}`?", escape_control(last)).unwrap_or_default();
    Some(out)
}

/// `names` as a list, at most `cap` of them, the rest counted:
/// `a, b, c, … (+4 more)`. Control bytes escaped.
pub fn listed<'a>(names: impl IntoIterator<Item = &'a str>, cap: usize) -> String {
    let mut out = String::new();
    let mut more = 0usize;
    for (i, n) in names.into_iter().enumerate() {
        if i >= cap {
            more += 1;
            continue;
        }
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&escape_control(n));
    }
    if more > 0 {
        write!(out, ", … (+{more} more)").unwrap_or_default();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s<'a>(input: &str, cands: &[&'a str]) -> Vec<&'a str> {
        suggest(input, cands.iter().copied())
    }

    // The D4 adversarial table, one test per row.

    #[test]
    fn suggest_one_char_gets_no_edit_distance_match() {
        assert!(s("a", &["ab"]).is_empty());
    }

    #[test]
    fn suggest_two_chars_get_no_transposition() {
        assert!(s("ab", &["ba"]).is_empty());
    }

    #[test]
    fn suggest_one_char_case_only() {
        assert_eq!(s("A", &["a"]), ["a"]);
    }

    #[test]
    fn suggest_case_only_is_always_suggested() {
        assert_eq!(s("Build", &["build"]), ["build"]);
        // However long the name: the edit-distance bound does not apply.
        let long = "Some_Very_Long_Option_Name_That_Differs_Only_In_Case";
        let lower = long.to_ascii_lowercase();
        assert_eq!(s(long, &[lower.as_str()]), [lower.as_str()]);
    }

    #[test]
    fn suggest_transposition() {
        assert_eq!(s("bulid", &["build"]), ["build"]);
    }

    #[test]
    fn suggest_ties_are_all_listed() {
        assert_eq!(s("abc", &["abe", "abd"]), ["abd", "abe"]);
    }

    #[test]
    fn suggest_one_clear_winner_alone() {
        // `deps` is one edit away, `dev` more: only the winner.
        assert_eq!(s("dpes", &["dev", "deps", "out"]), ["deps"]);
    }

    #[test]
    fn suggest_exact_match_is_no_suggestion() {
        assert!(s("abcd", &["abcd"]).is_empty());
        // Not even with near misses beside it.
        assert!(s("abcd", &["abce", "abcd"]).is_empty());
    }

    #[test]
    fn suggest_empty_input() {
        assert!(s("", &["a", "build", ""]).is_empty());
    }

    #[test]
    fn suggest_nfd_input_nfc_candidate() {
        let nfd = "cafe\u{301}";
        let nfc = "caf\u{e9}";
        assert_eq!(s(nfd, &[nfc]), [nfc]);
    }

    #[test]
    fn suggest_diacritic_is_one_edit() {
        assert_eq!(s("na\u{ef}ve", &["naive"]), ["naive"]);
    }

    #[test]
    fn suggest_over_name_cap() {
        let a = "x".repeat(300);
        let b = format!("{}y", "x".repeat(299));
        assert!(s(&a, &[b.as_str()]).is_empty());
        // A long candidate is skipped for a short input too.
        let long = format!("ab{}", "c".repeat(200));
        assert!(s("abc", &[long.as_str()]).is_empty());
    }

    #[test]
    fn suggest_control_byte_input_shown_escaped() {
        assert_eq!(s("a\x1bbc", &["abc"]), ["abc"]);
        // The input is shown escaped wherever it is named.
        assert_eq!(escape_control("a\x1bbc"), "a\\x1bbc");
        // A candidate's control bytes are escaped in the hint.
        let hints = did_you_mean(&["ab\x1bc"]);
        assert_eq!(hints.first().map(|h| h.text.as_str()), Some("ab\\x1bc"));
        let text = did_you_mean_text(&["ab\x07"]).unwrap();
        assert_eq!(text, "did you mean `ab\\x07`?");
    }

    #[test]
    fn suggest_duplicates_count_once() {
        assert_eq!(s("abx", &["abc", "abc"]), ["abc"]);
    }

    #[test]
    fn suggest_unrelated_is_nothing() {
        assert!(s("zzzz", &["build"]).is_empty());
    }

    #[test]
    fn suggest_over_candidate_cap_counts_only() {
        let names: Vec<String> = (0..6_000).map(|i| format!("name{i}")).collect();
        let got = suggestions("name1x", names.iter().map(String::as_str));
        assert!(got.names.is_empty());
        assert_eq!(got.over_cap, Some(6_000));
        let hints = got.hints();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].text.contains("6000 candidates"), "{:?}", hints);
        assert_eq!(hints[0].kind, HintKind::Other);
    }

    #[test]
    fn suggest_counts_distances_within_cap() {
        // 5,000 candidates of 128 chars: a short input computes no distance
        // at all, the prefilter rejects every one.
        let names: Vec<String> = (0..CANDIDATES_CAP).map(|i| format!("{i:0>128}")).collect();
        let mut n = 0;
        let got = rank("deps", names.iter().map(String::as_str), &mut n);
        assert!(got.names.is_empty() && got.over_cap.is_none());
        assert_eq!(n, 0);
        // A 128-char input computes one distance per candidate within its
        // length window, never more than the cap.
        let input = format!("{:0>127}x", 1);
        let mut n = 0;
        let got = rank(&input, names.iter().map(String::as_str), &mut n);
        assert!(!got.names.is_empty());
        assert!(n <= CANDIDATES_CAP, "{n}");
        // Mixed lengths: only the candidates whose length could match.
        let mixed: Vec<String> = (0..CANDIDATES_CAP)
            .map(|i| "a".repeat(1 + i % 100))
            .collect();
        let mut n = 0;
        let input = format!("b{}", "a".repeat(19));
        rank(&input, mixed.iter().map(String::as_str), &mut n);
        // Lengths 15..=25 and within them only distinct names: 11 names.
        assert!(n <= 11, "{n}");
    }

    #[test]
    fn did_you_mean_text_lists_ties() {
        assert_eq!(did_you_mean_text(&[]), None);
        assert_eq!(
            did_you_mean_text(&["a", "b", "c"]).as_deref(),
            Some("did you mean `a`, `b` or `c`?")
        );
    }

    #[test]
    fn listed_counts_the_rest() {
        assert_eq!(listed(["a", "b"], 5), "a, b");
        assert_eq!(listed(["a", "b", "c"], 2), "a, b, … (+1 more)");
    }
}
