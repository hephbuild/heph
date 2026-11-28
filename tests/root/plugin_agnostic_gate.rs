//! U27, structurally: the engine knows no plugin.
//!
//! `crates/engine`, `crates/core` and `crates/plugin` handle what providers
//! and drivers report; none of them may know a particular plugin's files,
//! language or name. A culprit file is "a source file a provider reported",
//! never "a BUILD file", and wording that needs a plugin's vocabulary comes
//! from the plugin. A branch on `.bzl`, an identifier named after Starlark,
//! or a message that says "BUILD" is the engine growing knowledge of the
//! buildfile plugin, and nothing else in the tree would notice: every test
//! runs that plugin, so the coupling always holds there.
//!
//! What it scans: the non-test code of those crates' `src/`. Comments may use
//! the buildfile plugin as an example, and tests may use `d/BUILD` as a
//! fixture path; code and string literals may not name it. Test code is a
//! `tests.rs` file, anything under a `tests/` directory, and any item
//! annotated with a `#[cfg(...)]` that mentions `test`.
//!
//! Cheap on purpose: a directory walk and a small lexer, no build.

use super::execrunner_gate::{repo_root, rust_files};

/// The crates that must stay plugin-agnostic.
const AGNOSTIC: &[&str] = &["crates/engine", "crates/core", "crates/plugin"];

/// Words an identifier or a path in code may not contain (compared in lower
/// case).
const CODE_WORDS: &[&str] = &["starlark", "buildfile", "build_file", "bzl"];

/// What a string literal may not contain: the buildfile plugin's file names,
/// extensions and language.
fn literal_names_a_plugin(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if ["starlark", "buildfile", "build file", ".bzl"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return true;
    }
    // An extension ends a file name: what precedes it is the name.
    let star_ext = s.match_indices(".star").any(|(i, w)| {
        !s.get(i + w.len()..)
            .and_then(|a| a.chars().next())
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    word_at(s, "BUILD") || star_ext
}

/// Whether `word` occurs in `s` not inside a longer identifier
/// (`d/BUILD`, `"BUILD"`, but not `BUILDER` or `REBUILD`).
fn word_at(s: &str, word: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    s.match_indices(word).any(|(i, _)| {
        let before = s.get(..i).and_then(|b| b.chars().next_back());
        let after = s.get(i + word.len()..).and_then(|a| a.chars().next());
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// One file's non-test code, split into what the gate checks: the code with
/// comments and literals removed, and each string literal's contents, each
/// with its 1-based line.
#[derive(Default)]
struct Scanned {
    code: Vec<(usize, String)>,
    strings: Vec<(usize, String)>,
    /// The modules declared out of line under a test cfg
    /// (`#[cfg(test)] mod tests;`): their files are test code too.
    test_mods: Vec<String>,
}

/// A small Rust lexer: comments (nested block comments too), string, byte
/// string and raw string literals, char literals (told from lifetimes), and
/// the braces that close a `#[cfg(test)]` item.
fn scan(text: &str) -> Scanned {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Scanned::default();
    let mut line = 1;
    let mut code = String::new();
    let mut i = 0;
    // Brace and paren depth of the code, and a pending `#[cfg(test)]`: the
    // depth its item starts at, until that item's `{ .. }` or its `;`/`,`.
    let mut depth = 0usize;
    let mut parens = 0usize;
    let mut pending: Option<(usize, usize)> = None;
    let mut skip_until: Option<usize> = None;
    let flush = |out: &mut Scanned, code: &mut String, line: usize| {
        if !code.trim().is_empty() {
            out.code.push((line, std::mem::take(code)));
        }
        code.clear();
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let skipping = skip_until.is_some() || pending.is_some();
        match c {
            '\n' => {
                if !skipping {
                    flush(&mut out, &mut code, line);
                }
                code.clear();
                line += 1;
                i += 1;
            }
            '/' if next == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut nest = 0usize;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        nest += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        nest -= 1;
                        i += 2;
                        if nest == 0 {
                            break;
                        }
                    } else {
                        if chars[i] == '\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
            }
            '"' | 'r' | 'b' if starts_literal(&chars, i) => {
                let start_line = line;
                let (s, end, lines) = read_literal(&chars, i);
                if !skipping {
                    out.strings.push((start_line, s));
                }
                code.push_str("\"\"");
                line += lines;
                i = end;
            }
            '\'' => {
                // A char literal (`'{'`, `'\''`, `'\u{7b}'`) or a lifetime.
                if next == Some('\\') {
                    let mut j = i + 2;
                    while j < chars.len() && chars[j] != '\'' {
                        j += 1;
                    }
                    i = j + 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    code.push(c);
                    i += 1;
                }
            }
            _ => {
                if c.is_alphanumeric() || c == '_' {
                    // A whole identifier, so `r`/`b` inside one never starts a literal.
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        code.push(chars[i]);
                        i += 1;
                    }
                    continue;
                }
                code.push(c);
                i += 1;
                match c {
                    ']' if pending.is_none() && skip_until.is_none() => {
                        if let Some(attr) = code.rfind("#[cfg(") {
                            let attr = code.get(attr..).unwrap_or_default();
                            if word_at(attr, "test") && !attr.contains("not(test") {
                                pending = Some((depth, parens));
                            }
                        }
                    }
                    '{' => {
                        if let Some((d, _)) = pending.take() {
                            skip_until = Some(d);
                        }
                        depth += 1;
                    }
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if skip_until == Some(depth) {
                            skip_until = None;
                            code.clear();
                        }
                    }
                    '(' => parens += 1,
                    ')' => parens = parens.saturating_sub(1),
                    ';' | ',' if pending.is_some_and(|(d, p)| d == depth && p == parens) => {
                        pending = None;
                        if let Some((_, name)) = code.split_once("mod ") {
                            let name = name.trim_end_matches(';').trim();
                            if !name.is_empty() {
                                out.test_mods.push(name.to_owned());
                            }
                        }
                        code.clear();
                    }
                    _ => {}
                }
            }
        }
    }
    if skip_until.is_none() {
        flush(&mut out, &mut code, line);
    }
    out
}

/// Whether a literal starts at `i`: `"`, `r"`, `r#"`, `b"`, `br"`, `br#"`
/// (the caller has already ruled out the middle of an identifier).
fn starts_literal(chars: &[char], i: usize) -> bool {
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) == Some(&'r') {
        j += 1;
        while chars.get(j) == Some(&'#') {
            j += 1;
        }
    }
    chars.get(j) == Some(&'"')
}

/// The literal starting at `i`: its contents, the index past it, and how
/// many newlines it spans.
fn read_literal(chars: &[char], i: usize) -> (String, usize, usize) {
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    let raw = chars.get(j) == Some(&'r');
    let mut hashes = 0;
    if raw {
        j += 1;
        while chars.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
    }
    j += 1; // the opening quote
    let mut s = String::new();
    let mut lines = 0;
    while j < chars.len() {
        let c = chars[j];
        if c == '\n' {
            lines += 1;
        }
        if !raw && c == '\\' {
            s.push(c);
            if let Some(&e) = chars.get(j + 1) {
                if e == '\n' {
                    lines += 1;
                }
                s.push(e);
            }
            j += 2;
            continue;
        }
        if c == '"' && (0..hashes).all(|h| chars.get(j + 1 + h) == Some(&'#')) {
            return (s, j + 1 + hashes, lines);
        }
        s.push(c);
        j += 1;
    }
    (s, j, lines)
}

fn is_test_file(rel: &std::path::Path) -> bool {
    rel.file_name().is_some_and(|n| n == "tests.rs")
        || rel.components().any(|c| c.as_os_str() == "tests")
}

#[test]
fn agnostic_crates_name_no_plugin() {
    let root = repo_root();
    let mut found = Vec::new();
    for krate in AGNOSTIC {
        let mut files = Vec::new();
        rust_files(&root.join(krate).join("src"), &mut files);
        for path in files {
            let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            if is_test_file(&rel) {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            found.push((rel, scan(&text)));
        }
    }
    // A module declared under a test cfg is test code wherever its file is:
    // `<dir>/<name>.rs` or `<dir>/<name>/`, `<dir>` being the declaring
    // file's own module directory.
    let test_dirs: Vec<std::path::PathBuf> = found
        .iter()
        .flat_map(|(rel, s)| {
            let parent = rel.parent().unwrap_or(std::path::Path::new(""));
            let dir = match rel.file_stem().and_then(|s| s.to_str()) {
                Some("mod" | "lib" | "main") | None => parent.to_path_buf(),
                Some(stem) => parent.join(stem),
            };
            s.test_mods.iter().map(move |m| dir.join(m))
        })
        .collect();
    let mut offenders = Vec::new();
    let mut scanned = 0;
    for (rel, found) in &found {
        if test_dirs
            .iter()
            .any(|d| rel.starts_with(d) || *rel == d.with_extension("rs"))
        {
            continue;
        }
        scanned += 1;
        for (n, code) in &found.code {
            let lower = code.to_ascii_lowercase();
            if let Some(w) = CODE_WORDS.iter().find(|w| lower.contains(*w)) {
                offenders.push(format!("{}:{n}: code names `{w}`", rel.display()));
            }
        }
        for (n, s) in &found.strings {
            if literal_names_a_plugin(s) {
                offenders.push(format!("{}:{n}: literal {s:?}", rel.display()));
            }
        }
    }
    assert!(
        scanned > 50,
        "scanned only {scanned} files — the crate layout changed and this gate \
         now covers almost nothing"
    );
    assert!(
        offenders.is_empty(),
        "the engine, core and plugin crates must know no plugin (U27, \
         .claude/architecture.md). These name the buildfile plugin's files, \
         language or name in code:\n  {}\n\nSay \"source file\" and quote the \
         path a provider reported; wording that needs a plugin's vocabulary \
         comes from the plugin. A comment may use it as an example.",
        offenders.join("\n  ")
    );
}

/// The lexer itself: what it must skip, and what it must still see.
#[test]
fn scan_skips_comments_and_test_items() {
    let src = r####"
// a comment naming BUILD
/* a block /* nested */ naming starlark */
fn keep() { let s = "d/BUILD"; let c = '{'; let l: &'static str = "x"; }
#[cfg(test)]
mod tests { fn t() { let s = "p/BUILD"; let starlark = 1; } }
#[cfg(any(test, feature = "test-support"))]
pub fn helper() -> &'static str { "q/BUILD" }
#[cfg(test)]
use std::path::Path;
#[cfg(any(test, feature = "test-support"))]
pub mod fixture;
struct S {
    #[cfg(test)]
    starlark_only: u8,
    kept: u8,
}
fn after() { let raw = r#"a "quoted" .bzl"#; let buildfile = 2; }
"####;
    let s = scan(src);
    let strings: Vec<&str> = s.strings.iter().map(|(_, s)| s.as_str()).collect();
    // An attribute's strings are code, and scanned.
    assert_eq!(
        strings,
        [
            "d/BUILD",
            "x",
            "test-support",
            "test-support",
            "a \"quoted\" .bzl"
        ],
        "{strings:?}"
    );
    assert_eq!(s.test_mods, ["fixture"]);
    let code: String = s.code.iter().map(|(_, c)| c.as_str()).collect();
    assert!(code.contains("fn after"), "{code}");
    assert!(code.contains("kept"), "{code}");
    assert!(code.contains("let buildfile"), "{code}");
    assert!(!code.contains("starlark"), "{code}");
    assert!(!code.contains("helper"), "{code}");
    assert!(!code.contains("Path"), "{code}");
    assert!(literal_names_a_plugin("evaluating d/BUILD"));
    assert!(literal_names_a_plugin("lib/defs.star"));
    assert!(!literal_names_a_plugin("REBUILD the BUILDER"));
    assert!(!literal_names_a_plugin("start"));
}
