//! `grep`, over ripgrep's search engine.
//!
//! macOS has no `grep -P`, and `--include`, `-z`, recursion through symlinks
//! and the colour defaults all differ between the two hosts. This is a POSIX
//! front-end over [`grep_searcher`] and [`grep_regex`] — the engine ripgrep
//! uses — so the matching is battle-tested and the flag surface is ours.
//!
//! Two deliberate departures from GNU, both because the alternative is worse:
//!
//! * **No backreferences or lookaround.** The `regex` crate does not have them,
//!   by design. A pattern using them is a clear error naming the construct
//!   rather than a silently different match.
//! * **Never colourised.** Output goes into build logs and gets parsed; a
//!   `--color=auto` that guessed from a tty would make a recipe's behaviour
//!   depend on how it was invoked.

use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// POSIX: 0 when something matched, 1 when nothing did, 2 on error.
const MATCHED: i32 = 0;
const NO_MATCH: i32 = 1;
const ERROR: i32 = 2;

#[derive(Debug, Default)]
struct Options {
    patterns: Vec<String>,
    files: Vec<PathBuf>,
    fixed: bool,
    ignore_case: bool,
    invert: bool,
    line_number: bool,
    count: bool,
    files_with_matches: bool,
    files_without_match: bool,
    quiet: bool,
    word: bool,
    line_regexp: bool,
    recursive: bool,
    no_filename: bool,
    with_filename: Option<bool>,
    no_messages: bool,
    max_count: Option<u64>,
}

fn usage(program: &str) -> String {
    format!(
        "usage: {program} [-EFivnclLqwxrhHs] [-m NUM] [-e PATTERN]... [-f FILE]... [PATTERN] [FILE]...\n\
         \n\
         heph's builtin grep. Extended regular expressions; no backreferences or\n\
         lookaround, and never colourised. `heph tool coreutils which grep` explains\n\
         which grep a target gets."
    )
}

/// Parse argv the way POSIX utilities do: options until the first operand,
/// with `--` ending them.
///
/// Hand-rolled rather than clap, because `grep -e -v` must treat `-v` as the
/// *pattern*, and a declarative parser fights that.
fn parse(argv: &[OsString]) -> Result<Options, (i32, String)> {
    let program = argv
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or("grep")
        .to_string();
    let mut o = Options::default();
    let mut pattern_given = false;
    let mut operands: Vec<PathBuf> = Vec::new();
    let mut only_operands = false;

    let mut it = argv.iter().skip(1).peekable();
    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        if only_operands || arg == "-" || !arg.starts_with('-') || arg.len() == 1 {
            if pattern_given || !o.patterns.is_empty() {
                operands.push(PathBuf::from(raw));
            } else {
                o.patterns.push(arg);
                pattern_given = true;
            }
            continue;
        }
        if arg == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "help" => return Err((MATCHED, usage(&program))),
                "fixed-strings" => o.fixed = true,
                "extended-regexp" => {}
                "ignore-case" => o.ignore_case = true,
                "invert-match" => o.invert = true,
                "line-number" => o.line_number = true,
                "count" => o.count = true,
                "files-with-matches" => o.files_with_matches = true,
                "files-without-match" => o.files_without_match = true,
                "quiet" | "silent" => o.quiet = true,
                "word-regexp" => o.word = true,
                "line-regexp" => o.line_regexp = true,
                "recursive" => o.recursive = true,
                "no-filename" => o.no_filename = true,
                "with-filename" => o.with_filename = Some(true),
                "no-messages" => o.no_messages = true,
                other => {
                    return Err((
                        ERROR,
                        format!(
                            "{program}: unrecognized option '--{other}'\n{}",
                            usage(&program)
                        ),
                    ));
                }
            }
            continue;
        }

        // A cluster of short options; `-e`, `-f` and `-m` take a value, which
        // may be attached (`-m3`) or the next argument (`-m 3`).
        let mut chars = arg.chars().skip(1).peekable();
        while let Some(c) = chars.next() {
            let mut take_value = |chars: &mut std::iter::Peekable<_>| -> Option<String> {
                let rest: String = std::iter::from_fn(|| chars.next()).collect();
                if !rest.is_empty() {
                    return Some(rest);
                }
                it.next().map(|v| v.to_string_lossy().into_owned())
            };
            match c {
                'E' => {}
                'F' => o.fixed = true,
                'i' | 'y' => o.ignore_case = true,
                'v' => o.invert = true,
                'n' => o.line_number = true,
                'c' => o.count = true,
                'l' => o.files_with_matches = true,
                'L' => o.files_without_match = true,
                'q' => o.quiet = true,
                'w' => o.word = true,
                'x' => o.line_regexp = true,
                'r' | 'R' => o.recursive = true,
                'h' => o.no_filename = true,
                'H' => o.with_filename = Some(true),
                's' => o.no_messages = true,
                'e' => match take_value(&mut chars) {
                    Some(p) => {
                        o.patterns.push(p);
                        pattern_given = true;
                    }
                    None => {
                        return Err((
                            ERROR,
                            format!("{program}: option requires an argument -- e"),
                        ));
                    }
                },
                'f' => match take_value(&mut chars) {
                    Some(path) => match std::fs::read_to_string(&path) {
                        Ok(body) => {
                            o.patterns.extend(body.lines().map(str::to_string));
                            pattern_given = true;
                        }
                        Err(e) => {
                            return Err((ERROR, format!("{program}: {path}: {e}")));
                        }
                    },
                    None => {
                        return Err((
                            ERROR,
                            format!("{program}: option requires an argument -- f"),
                        ));
                    }
                },
                'm' => match take_value(&mut chars).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) => o.max_count = Some(n),
                    None => return Err((ERROR, format!("{program}: invalid max count"))),
                },
                'P' => {
                    return Err((
                        ERROR,
                        format!(
                            "{program}: -P (perl regexp) is not supported — heph's grep uses the \
                             `regex` engine, which has no backreferences or lookaround. Rewrite \
                             the pattern as an extended regular expression."
                        ),
                    ));
                }
                other => {
                    return Err((
                        ERROR,
                        format!("{program}: invalid option -- {other}\n{}", usage(&program)),
                    ));
                }
            }
        }
    }

    if o.patterns.is_empty() {
        return Err((ERROR, usage(&program)));
    }
    o.files = operands;
    Ok(o)
}

/// Whether each matching line is prefixed with its file name.
///
/// GNU's rule, which recipes depend on: on by default once more than one file
/// is in play (including anything reached by `-r`), off for a single file,
/// and forced either way by `-H`/`-h`.
fn show_filenames(o: &Options, file_count: usize) -> bool {
    if o.no_filename {
        return false;
    }
    o.with_filename
        .unwrap_or(file_count > 1 || (o.recursive && file_count > 0))
}

fn build_matcher(o: &Options) -> anyhow::Result<grep_regex::RegexMatcher> {
    let patterns: Vec<String> = if o.fixed {
        o.patterns.iter().map(|p| regex_syntax::escape(p)).collect()
    } else {
        o.patterns.clone()
    };
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(o.ignore_case)
        .word(o.word)
        .line_terminator(Some(b'\n'));
    let joined = if o.line_regexp {
        patterns
            .iter()
            .map(|p| format!("^(?:{p})$"))
            .collect::<Vec<_>>()
    } else {
        patterns
    };
    b.build_literals(&joined)
        .or_else(|_| b.build(&joined.join("|")))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Every path to search, with `-r` expanded.
fn collect_paths(o: &Options) -> Vec<PathBuf> {
    if !o.recursive {
        return o.files.clone();
    }
    let mut out = Vec::new();
    for root in &o.files {
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() {
                out.push(entry.into_path());
            }
        }
    }
    out
}

pub fn main(argv: Vec<OsString>) -> i32 {
    let o = match parse(&argv) {
        Ok(o) => o,
        Err((code, msg)) => {
            if code == MATCHED {
                println!("{msg}");
            } else {
                eprintln!("{msg}");
            }
            return code;
        }
    };
    let program = argv
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or("grep")
        .to_string();

    let matcher = match build_matcher(&o) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{program}: {e}");
            return ERROR;
        }
    };

    let paths = collect_paths(&o);
    let with_names = show_filenames(&o, paths.len());
    // Line numbers are always *counted*, and only printed under `-n`: the
    // `UTF8` sink asks the match for its line number unconditionally and errors
    // with "line numbers not enabled" if the searcher was not tracking them.
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .invert_match(o.invert)
        .build();

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut any_match = false;
    let mut had_error = false;

    // No path operands means stdin, which is how `... | grep x` works.
    let from_stdin = paths.is_empty();
    let targets: Vec<Option<&Path>> = if from_stdin {
        vec![None]
    } else {
        paths.iter().map(|p| Some(p.as_path())).collect()
    };

    for target in targets {
        let mut count: u64 = 0;
        let label = target.map(|p| p.display().to_string());
        let mut emit = |lnum: Option<u64>, line: &str| -> std::io::Result<()> {
            if o.quiet || o.count || o.files_with_matches || o.files_without_match {
                return Ok(());
            }
            if with_names && let Some(name) = &label {
                write!(out, "{name}:")?;
            }
            if let Some(n) = lnum {
                write!(out, "{n}:")?;
            }
            out.write_all(line.as_bytes())?;
            if !line.ends_with('\n') {
                out.write_all(b"\n")?;
            }
            Ok(())
        };

        let sink = UTF8(|lnum, line| {
            count += 1;
            emit(o.line_number.then_some(lnum), line)?;
            // `-q` and `-l` only need to know *whether* something matched, and
            // `-m` caps the count: stopping early is the difference between
            // reading a byte and reading a gigabyte.
            let stop = o.quiet || o.files_with_matches || o.max_count.is_some_and(|m| count >= m);
            Ok(!stop)
        });

        let res = match target {
            Some(path) => searcher.search_path(&matcher, path, sink),
            None => searcher.search_reader(&matcher, std::io::stdin().lock(), sink),
        };
        if let Err(e) = res {
            if !o.no_messages {
                let name = label
                    .clone()
                    .unwrap_or_else(|| "(standard input)".to_string());
                eprintln!("{program}: {name}: {e}");
            }
            had_error = true;
            continue;
        }

        if count > 0 {
            any_match = true;
        }
        // A write failure here is a closed pipe in practice (`grep x | head`),
        // which is not something to report — but it is a reason to stop writing
        // rather than to keep going and ignore it.
        let summary = (|| -> std::io::Result<()> {
            if o.count {
                if with_names && let Some(name) = &label {
                    write!(out, "{name}:")?;
                }
                writeln!(out, "{count}")?;
            }
            if o.files_with_matches
                && count > 0
                && let Some(name) = &label
            {
                writeln!(out, "{name}")?;
            }
            if o.files_without_match
                && count == 0
                && let Some(name) = &label
            {
                writeln!(out, "{name}")?;
            }
            Ok(())
        })();
        if summary.is_err() {
            return if any_match { MATCHED } else { NO_MATCH };
        }
        if o.quiet && any_match {
            return MATCHED;
        }
    }

    drop(out.flush());
    if had_error && !any_match {
        return ERROR;
    }
    if any_match { MATCHED } else { NO_MATCH }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_first_operand_is_the_pattern_and_the_rest_are_files() {
        let o = parse(&argv(&["grep", "needle", "a.txt", "b.txt"])).expect("parse");
        assert_eq!(o.patterns, vec!["needle"]);
        assert_eq!(o.files.len(), 2);
    }

    #[test]
    fn dash_e_takes_the_next_argument_even_when_it_looks_like_a_flag() {
        // `grep -e -v file` searches for the literal "-v". A declarative parser
        // would take it as the invert flag and search for "file".
        let o = parse(&argv(&["grep", "-e", "-v", "f.txt"])).expect("parse");
        assert_eq!(o.patterns, vec!["-v"]);
        assert!(!o.invert, "-v after -e is the pattern, not the flag");
        assert_eq!(o.files.len(), 1);
    }

    #[test]
    fn short_options_cluster_and_take_attached_values() {
        let o = parse(&argv(&["grep", "-inm3", "x", "f"])).expect("parse");
        assert!(o.ignore_case && o.line_number);
        assert_eq!(o.max_count, Some(3));
        assert_eq!(o.patterns, vec!["x"]);
    }

    #[test]
    fn perl_regexp_is_refused_with_a_reason() {
        // -P silently missing on macOS is one of the divergences this exists to
        // remove; failing with an explanation beats failing with "invalid option".
        let (code, msg) = parse(&argv(&["grep", "-P", "\\d+", "f"])).expect_err("must refuse");
        assert_eq!(code, ERROR);
        assert!(msg.contains("backreferences"), "{msg}");
    }

    #[test]
    fn filenames_follow_gnus_rule() {
        let one = parse(&argv(&["grep", "x", "a"])).expect("parse");
        assert!(!show_filenames(&one, 1), "a single file gets no prefix");
        let two = parse(&argv(&["grep", "x", "a", "b"])).expect("parse");
        assert!(show_filenames(&two, 2), "several files get a prefix");
        let forced = parse(&argv(&["grep", "-H", "x", "a"])).expect("parse");
        assert!(show_filenames(&forced, 1), "-H forces it on");
        let suppressed = parse(&argv(&["grep", "-h", "x", "a", "b"])).expect("parse");
        assert!(!show_filenames(&suppressed, 2), "-h forces it off");
    }

    #[test]
    fn fixed_strings_are_escaped_not_compiled() {
        // `grep -F 'a.b'` must not match "axb".
        let o = parse(&argv(&["grep", "-F", "a.b", "f"])).expect("parse");
        let m = build_matcher(&o).expect("matcher");
        use grep_matcher::Matcher as _;
        assert!(m.is_match(b"a.b").expect("match"));
        assert!(!m.is_match(b"axb").expect("match"));
    }

    #[test]
    fn line_regexp_anchors_the_whole_line() {
        let o = parse(&argv(&["grep", "-x", "ab", "f"])).expect("parse");
        let m = build_matcher(&o).expect("matcher");
        use grep_matcher::Matcher as _;
        assert!(m.is_match(b"ab").expect("match"));
        assert!(!m.is_match(b"xaby").expect("match"));
    }

    #[test]
    fn no_pattern_is_a_usage_error_not_a_panic() {
        let (code, _) = parse(&argv(&["grep"])).expect_err("must fail");
        assert_eq!(code, ERROR);
    }
}
