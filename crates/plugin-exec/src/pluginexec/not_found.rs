//! The "not on this target's PATH" hint (error-reporting D8, RUN-06/07).
//!
//! A shell that cannot find a program prints a not-found line and exits 127.
//! [`NotFoundScan`] reads a run's stderr as it is teed and keeps the program
//! words of those lines; [`path_hint`] says them, for a run that exited 127.
//! Both rules fail closed: a line that is not exactly a shell's not-found
//! line (a localized one, a script's own words) names nothing, and a run
//! that did not exit 127 gets no hint whatever its log says.

use hcore::diagnostic::{Hint, HintKind};

/// The longest stderr line looked at. A shell's not-found line is short; a
/// longer one is skipped whole, so a line is never matched on a fragment.
const LINE_CAP: usize = 512;

/// The longest program word kept.
const WORD_CAP: usize = 128;

/// How many distinct words are kept. Past it, a hint says "or more".
const WORDS_CAP: usize = 32;

/// How many words a hint names; the rest are counted.
const NAMED: usize = 3;

/// The program words of a run's not-found lines, in first-seen order and
/// distinct, from its stderr fed chunk by chunk.
#[derive(Debug, Default)]
pub(crate) struct NotFoundScan {
    /// The current line so far, when it is within [`LINE_CAP`].
    partial: Vec<u8>,
    /// The current line went past [`LINE_CAP`]: skipped up to its newline.
    skipping: bool,
    words: Vec<String>,
    /// A distinct word arrived with [`WORDS_CAP`] already kept.
    overflow: bool,
}

impl NotFoundScan {
    /// Feed the next chunk of stderr.
    pub(crate) fn push(&mut self, mut chunk: &[u8]) {
        while let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            let (head, rest) = chunk.split_at(i);
            self.extend(head);
            self.end_line();
            chunk = rest.get(1..).unwrap_or_default();
        }
        self.extend(chunk);
    }

    /// Stderr ended: a last line with no newline is a line too.
    pub(crate) fn finish(&mut self) {
        self.end_line();
    }

    /// The words found, first seen first, and whether more were dropped.
    pub(crate) fn words(&self) -> (&[String], bool) {
        (&self.words, self.overflow)
    }

    fn extend(&mut self, bytes: &[u8]) {
        if self.skipping {
            return;
        }
        if self.partial.len() + bytes.len() > LINE_CAP {
            self.partial.clear();
            self.skipping = true;
        } else {
            self.partial.extend_from_slice(bytes);
        }
    }

    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.partial);
        self.skipping = false;
        let Some(word) = std::str::from_utf8(&line).ok().and_then(not_found_word) else {
            return;
        };
        if self.words.iter().any(|w| w == word) {
            return;
        }
        if self.words.len() == WORDS_CAP {
            self.overflow = true;
        } else {
            self.words.push(word.to_owned());
        }
    }
}

/// The program `line` says a shell could not find: bash's
/// `bash: line 1: jq: command not found` or dash's `sh: 1: jq: not found`.
/// The line has at least three `": "`-separated fields (the shell, where,
/// the word), and the word is a non-empty run of non-space, non-control
/// characters within [`WORD_CAP`].
fn not_found_word(line: &str) -> Option<&str> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let head = line
        .strip_suffix(": command not found")
        .or_else(|| line.strip_suffix(": not found"))?;
    let (before, word) = head.rsplit_once(": ")?;
    // Three fields: the shell, where, the word. Two (`bash: jq: …`) only
    // when the first is a shell's name or path, never a sentence.
    if before.is_empty() || (!before.contains(": ") && before.contains(' ')) {
        return None;
    }
    let ok = !word.is_empty()
        && word.len() <= WORD_CAP
        && !word.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then_some(word)
}

/// What a target's PATH holds besides its declared tools, for the hint's
/// wording.
#[derive(Debug)]
pub(crate) struct PathEnv {
    /// heph's builtin coreutils are on it.
    pub coreutils: bool,
    /// The driver's `path` option, joined; empty when it names none.
    pub search_path: String,
    /// The exec runner the target runs under.
    pub runner: Option<String>,
    /// The caller's own PATH is on it (an entrypoint run).
    pub caller_path: bool,
}

/// The hint for `words` (as [`NotFoundScan::words`] gives them) missing from
/// a target's PATH described by `env`; `None` for no words.
pub(crate) fn path_hint(words: &[String], overflow: bool, env: &PathEnv) -> Option<Hint> {
    if words.is_empty() {
        return None;
    }
    let named: Vec<String> = words.iter().take(NAMED).map(|w| format!("`{w}`")).collect();
    let more = words.len().saturating_sub(NAMED);
    let subject = if more > 0 {
        let or = if overflow { " or" } else { "" };
        format!("{} (+{more}{or} more)", named.join(", "))
    } else {
        and_list(&named)
    };
    let verb = if words.len() == 1 { "is" } else { "are" };
    let text = if env.caller_path {
        format!(
            "{subject} {verb} not on this run's PATH: the target's declared `tools`, then \
             the caller's PATH{builtins}. Declare it in `tools = [...]`, or put it on your PATH.",
            builtins = if env.coreutils {
                ", then heph's builtin coreutils"
            } else {
                ""
            },
        )
    } else {
        let mut sees = vec!["its declared `tools`".to_owned()];
        if env.coreutils {
            sees.push("heph's builtin coreutils".to_owned());
        }
        if !env.search_path.is_empty() {
            sees.push(format!("the driver's `path` ({})", env.search_path));
        }
        let never = match &env.runner {
            Some(runner) => {
                sees.push(format!("the environment of its exec runner `{runner}`"));
                "not the host PATH"
            }
            None => "never the host PATH",
        };
        let sees = and_list(&sees);
        let remedy = if env.runner.is_some() {
            "Declare it in `tools = [...]`, or add it to the runner's environment."
        } else {
            "Declare it in `tools = [...]`."
        };
        format!(
            "{subject} {verb} not on this target's PATH: a target sees only {sees}, {never}. \
             {remedy}"
        )
    };
    Some(Hint::new(HintKind::DeclareTool, text))
}

/// A spawn that failed with `ENOENT`, as the exec driver saw it.
#[derive(Debug)]
pub(crate) struct SpawnFailed<'a> {
    /// The target's program, as written.
    pub program: &'a str,
    /// ` (via exec runner …)` under a runner, else empty.
    pub via: &'a str,
    pub error: &'a std::io::Error,
    /// The target's PATH, as the driver describes it.
    pub path: &'a str,
    /// The directory the process was spawned in.
    pub cwd: &'a std::path::Path,
}

/// The message of a spawn `ENOENT` (RUN-07), and its hint. The hint is the
/// 127 hint for the program, when the program is a word looked up on PATH
/// (no `/`) and the target is not under a runner, whose wrapper is what
/// failed to spawn. The working directory is named only when it is missing:
/// a missing cwd fails the spawn with the same error.
pub(crate) fn spawn_not_found(s: &SpawnFailed<'_>, env: &PathEnv) -> (String, Option<Hint>) {
    let SpawnFailed {
        program,
        via,
        error,
        path,
        cwd,
    } = s;
    let looked = if env.caller_path {
        "not found on the run's PATH (the target's tools, then the caller's PATH, then heph's \
         builtin utilities). Call it by absolute path, or put it on your PATH."
            .to_owned()
    } else {
        format!(
            "not found on the target's PATH ({path}). Call it by absolute path, or name its \
             directory in the driver's `path` option in .hephconfig."
        )
    };
    let cwd_missing = if cwd.is_dir() {
        String::new()
    } else {
        format!(
            " The working directory {cwd:?} does not exist, which fails the spawn the same way."
        )
    };
    let message = format!("spawn child process {program:?}{via}: {error} — {looked}{cwd_missing}");
    let word = !program.is_empty() && !program.contains('/') && env.runner.is_none();
    let hint = if word {
        path_hint(&[(*program).to_owned()], false, env)
    } else {
        None
    };
    (message, hint)
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => items.join(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> PathEnv {
        PathEnv {
            coreutils: true,
            search_path: String::new(),
            runner: None,
            caller_path: false,
        }
    }

    /// The words `stderr` names, fed in `split`-byte chunks so a line split
    /// across chunks is a line.
    fn scan(stderr: &str, split: usize) -> (Vec<String>, bool) {
        let mut s = NotFoundScan::default();
        for c in stderr.as_bytes().chunks(split.max(1)) {
            s.push(c);
        }
        s.finish();
        let (w, o) = s.words();
        (w.to_vec(), o)
    }

    fn hint(stderr: &str, env: &PathEnv) -> Option<String> {
        let (w, o) = scan(stderr, 7);
        path_hint(&w, o, env).map(|h| h.text)
    }

    #[test]
    fn hint_127_bash_command_not_found() {
        let h = hint("bash: line 1: jq: command not found\n", &env()).expect("a hint");
        assert_eq!(
            h,
            "`jq` is not on this target's PATH: a target sees only its declared `tools` and \
             heph's builtin coreutils, never the host PATH. Declare it in `tools = [...]`."
        );
    }

    #[test]
    fn hint_127_dash_not_found() {
        let h = hint("sh: 1: jq: not found\n", &env()).expect("a hint");
        assert!(h.starts_with("`jq` is not on this target's PATH"), "{h}");
    }

    #[test]
    fn hint_127_script_exit_127_no_line() {
        assert_eq!(hint("doing work\nfailed: giving up\n", &env()), None);
        assert_eq!(hint("", &env()), None);
    }

    #[test]
    fn hint_127_jq_and_yq_both_named() {
        let h = hint(
            "bash: line 1: jq: command not found\nbash: line 2: yq: command not found\n\
             bash: line 3: jq: command not found\n",
            &env(),
        )
        .expect("a hint");
        assert!(
            h.starts_with("`jq` and `yq` are not on this target's PATH"),
            "{h}"
        );
    }

    #[test]
    fn hint_127_four_missing_three_named() {
        let h = hint(
            "x.sh: line 1: a: command not found\nx.sh: line 2: b: command not found\n\
             x.sh: line 3: c: command not found\nx.sh: line 4: d: command not found\n",
            &env(),
        )
        .expect("a hint");
        assert!(
            h.starts_with("`a`, `b`, `c` (+1 more) are not on this target's PATH"),
            "{h}"
        );
    }

    #[test]
    fn hint_127_localized_message_no_hint() {
        assert_eq!(hint("bash: jq : commande introuvable\n", &env()), None);
        assert_eq!(
            hint("bash: line 1: jq: Befehl nicht gefunden\n", &env()),
            None
        );
    }

    #[test]
    fn hint_127_coreutils_off_drops_builtins() {
        let h = hint(
            "bash: line 1: jq: command not found\n",
            &PathEnv {
                coreutils: false,
                ..env()
            },
        )
        .expect("a hint");
        assert_eq!(
            h,
            "`jq` is not on this target's PATH: a target sees only its declared `tools`, \
             never the host PATH. Declare it in `tools = [...]`."
        );
    }

    #[test]
    fn hint_127_under_runner_names_its_environment() {
        let h = hint(
            "bash: line 1: jq: command not found\n",
            &PathEnv {
                runner: Some("//tools/devenv:runner".to_owned()),
                ..env()
            },
        )
        .expect("a hint");
        assert!(
            h.contains(
                "the environment of its exec runner `//tools/devenv:runner`, not the host PATH"
            ),
            "{h}"
        );
        assert!(!h.contains("never the host PATH"), "{h}");
    }

    #[test]
    fn hint_127_caller_path_and_search_path() {
        let h = hint(
            "bash: jq: command not found\n",
            &PathEnv {
                caller_path: true,
                ..env()
            },
        )
        .expect("a hint");
        assert!(
            h.contains("then the caller's PATH, then heph's builtin coreutils"),
            "{h}"
        );
        let h = hint(
            "bash: jq: command not found\n",
            &PathEnv {
                search_path: "/usr/bin:/bin".to_owned(),
                ..env()
            },
        )
        .expect("a hint");
        assert!(
            h.contains("its declared `tools`, heph's builtin coreutils and the driver's `path` (/usr/bin:/bin), never the host PATH"),
            "{h}"
        );
    }

    fn spawn(program: &str, cwd: &std::path::Path, env: &PathEnv) -> (String, Option<Hint>) {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        spawn_not_found(
            &SpawnFailed {
                program,
                via: "",
                error: &error,
                path: "the target's tools and heph's builtin utilities",
                cwd,
            },
            env,
        )
    }

    #[test]
    fn hint_127_exec_spawn_enoent_cwd_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (message, hint) = spawn("jq", dir.path(), &env());
        assert!(!message.contains("working directory"), "{message}");
        let hint = hint.expect("a hint");
        assert_eq!(hint.kind, HintKind::DeclareTool);
        assert!(
            hint.text.starts_with("`jq` is not on this target's PATH"),
            "{}",
            hint.text
        );
    }

    #[test]
    fn hint_127_exec_spawn_enoent_cwd_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("gone");
        let (message, hint) = spawn("jq", &gone, &env());
        assert!(
            message.contains(&format!("The working directory {gone:?} does not exist")),
            "{message}"
        );
        assert!(hint.is_some());
    }

    /// A path is not looked up on PATH, and under a runner the wrapper is
    /// what failed to spawn: neither gets the PATH hint.
    #[test]
    fn spawn_enoent_of_a_path_or_a_runner_has_no_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(spawn("./bin/tool", dir.path(), &env()).1.is_none());
        let runner = PathEnv {
            runner: Some("//t:r".to_owned()),
            ..env()
        };
        assert!(spawn("jq", dir.path(), &runner).1.is_none());
    }

    /// Lines that look like a not-found line and are not one: each names
    /// nothing.
    #[test]
    fn not_found_word_near_misses() {
        for line in [
            "jq: command not found",                    // no shell field
            "error: not found",                         // two fields
            "make: *** target not found",               // no `: ` before the word
            "bash: line 1: : command not found",        // empty word
            "bash: line 1: j q: command not found",     // a space in the word
            "bash: line 1: jq\u{7}: command not found", // a control character
            "bash: line 1: jq: command not found here", // trailing words
            "the file foo: not found",                  // a sentence, not a shell
            "Error happened: jq: not found",            // a sentence before the word
        ] {
            assert_eq!(not_found_word(line), None, "{line:?}");
        }
        assert_eq!(
            not_found_word("bash: line 1: jq: command not found\r"),
            Some("jq")
        );
        assert_eq!(
            not_found_word("/w/p/run.sh: line 3: héphtool: command not found"),
            Some("héphtool")
        );
        let long = format!(
            "bash: line 1: {}: command not found",
            "x".repeat(WORD_CAP + 1)
        );
        assert_eq!(not_found_word(&long), None);
    }

    /// A line past [`LINE_CAP`] is skipped whole, and the next line is read.
    #[test]
    fn a_long_line_is_skipped_not_cut() {
        let stderr = format!(
            "{}bash: line 1: jq: command not found\nsh: 1: yq: not found\n",
            "y".repeat(LINE_CAP)
        );
        assert_eq!(scan(&stderr, 64).0, vec!["yq".to_owned()]);
    }

    #[test]
    fn words_past_the_cap_say_or_more() {
        let stderr: String = (0..WORDS_CAP + 2)
            .map(|i| format!("sh: 1: p{i}: not found\n"))
            .collect();
        let (w, o) = scan(&stderr, 100);
        assert_eq!(w.len(), WORDS_CAP);
        assert!(o);
        let h = path_hint(&w, o, &env()).expect("a hint").text;
        assert!(h.starts_with("`p0`, `p1`, `p2` (+29 or more) are"), "{h}");
    }
}
