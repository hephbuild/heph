//! `sed`, over the `regex` crate.
//!
//! The divergence this removes is the sharpest in the set: GNU's `-i` takes an
//! optional suffix and BSD's requires one, so `sed -i 's/a/b/' f` edits the file
//! on Linux and eats the next argument as a filename on macOS. `-E` versus `-r`,
//! and the `a`/`i`/`c` commands needing a backslash-newline on BSD, are close
//! behind.
//!
//! ## What this is not
//!
//! No embeddable POSIX sed exists in Rust, so this is written here — and the
//! engine underneath is the `regex` crate, which has **no backreferences and no
//! lookaround, by design**. That is a real gap, and the rule for it is: *reject
//! loudly, never approximate*. An unsupported construct is an error naming the
//! construct, never a silently different match. A wrong `sed` that keeps going
//! is far worse than one that stops.
//!
//! Basic regular expressions *are* supported, by translating them to the
//! extended syntax the engine speaks — `\(` becomes `(`, a bare `+` becomes
//! `\+`, and so on. Without that, the single most common idiom in real scripts
//! (`s/\(a\)\(b\)/\2\1/`) would not compile, and "reject loudly" would mean
//! rejecting almost everything.

use regex::Regex;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::PathBuf;

const OK: i32 = 0;
const ERROR: i32 = 1;

// ------------------------------------------------------------ BRE → ERE

/// Translate a POSIX basic regular expression into the extended syntax.
///
/// In a BRE the *escaped* forms are the special ones and the bare characters
/// are literal, which is exactly backwards from ERE. Everything inside a
/// bracket expression is passed through untouched, since its contents follow
/// neither set of rules.
fn bre_to_ere(pattern: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    let mut in_bracket = false;
    // Tracks `[^]a]` / `[]a]`, where a `]` in first position is a literal.
    let mut bracket_start = 0usize;
    let mut idx = 0usize;

    while let Some(c) = chars.next() {
        idx += 1;
        if in_bracket {
            out.push(c);
            if c == ']' && idx > bracket_start + 1 {
                in_bracket = false;
            }
            continue;
        }
        match c {
            '[' => {
                in_bracket = true;
                bracket_start = idx;
                out.push(c);
                if chars.peek() == Some(&'^') {
                    out.push('^');
                    chars.next();
                    idx += 1;
                    bracket_start += 1;
                }
            }
            '\\' => match chars.next() {
                // The escaped forms are BRE's specials; unescape them.
                Some(n @ ('(' | ')' | '{' | '}' | '|' | '+' | '?')) => {
                    idx += 1;
                    out.push(n);
                }
                Some(d @ '1'..='9') => {
                    anyhow::bail!(
                        "backreference \\{d} in a pattern is not supported — heph's sed uses \
                         the `regex` engine, which has no backreferences. Rewrite the pattern \
                         without it."
                    );
                }
                Some(n) => {
                    idx += 1;
                    out.push('\\');
                    out.push(n);
                }
                None => out.push('\\'),
            },
            // Bare, these are literal in a BRE.
            '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// Reject the constructs the engine cannot express, by name.
fn compile(pattern: &str, extended: bool, ignore_case: bool) -> anyhow::Result<Regex> {
    let translated = if extended {
        check_ere(pattern)?;
        pattern.to_string()
    } else {
        bre_to_ere(pattern)?
    };
    let mut b = regex::RegexBuilder::new(&translated);
    b.case_insensitive(ignore_case);
    b.build()
        .map_err(|e| anyhow::anyhow!("cannot compile pattern {pattern:?}: {e}"))
}

/// In ERE a backreference is written `\1` too, and it is equally unsupported.
fn check_ere(pattern: &str) -> anyhow::Result<()> {
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(d @ '1'..='9') = chars.peek().copied()
        {
            anyhow::bail!(
                "backreference \\{d} in a pattern is not supported — heph's sed uses the \
                 `regex` engine, which has no backreferences. Rewrite the pattern without it."
            );
        }
        if c == '\\' {
            chars.next();
        }
    }
    Ok(())
}

// -------------------------------------------------------------- script

#[derive(Debug)]
enum Addr {
    Line(u64),
    Last,
    Regex(Regex),
}

#[derive(Debug)]
enum Selector {
    All,
    One(Addr),
    Range(Addr, Addr),
}

#[derive(Debug)]
enum Kind {
    Substitute {
        re: Regex,
        replacement: String,
        global: bool,
        nth: usize,
        print: bool,
    },
    Delete,
    Print,
    Quit,
    Transliterate(Vec<char>, Vec<char>),
    LineNumber,
    Append(String),
    Insert(String),
    Change(String),
}

#[derive(Debug)]
struct Command {
    selector: Selector,
    negated: bool,
    kind: Kind,
    /// Set while a `Range` selector is open.
    in_range: std::cell::Cell<bool>,
}

struct Parser<'a> {
    src: std::iter::Peekable<std::str::Chars<'a>>,
    extended: bool,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, extended: bool) -> Self {
        Self {
            src: src.chars().peekable(),
            extended,
        }
    }

    fn skip_blank(&mut self) {
        while matches!(self.src.peek(), Some(' ' | '\t' | '\n' | ';')) {
            self.src.next();
        }
    }

    fn parse_addr(&mut self) -> anyhow::Result<Option<Addr>> {
        match self.src.peek().copied() {
            Some('$') => {
                self.src.next();
                Ok(Some(Addr::Last))
            }
            Some(d) if d.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(c) = self.src.peek().copied() {
                    if c.is_ascii_digit() {
                        n.push(c);
                        self.src.next();
                    } else {
                        break;
                    }
                }
                Ok(Some(Addr::Line(n.parse()?)))
            }
            Some('/') => {
                self.src.next();
                let pat = self.read_until('/')?;
                let icase = if self.src.peek() == Some(&'I') {
                    self.src.next();
                    true
                } else {
                    false
                };
                Ok(Some(Addr::Regex(compile(&pat, self.extended, icase)?)))
            }
            _ => Ok(None),
        }
    }

    /// Read to the next unescaped `delim`, honouring `\<delim>`.
    fn read_until(&mut self, delim: char) -> anyhow::Result<String> {
        let mut out = String::new();
        while let Some(c) = self.src.next() {
            if c == '\\' {
                match self.src.next() {
                    Some(n) if n == delim => out.push(delim),
                    Some(n) => {
                        out.push('\\');
                        out.push(n);
                    }
                    None => out.push('\\'),
                }
                continue;
            }
            if c == delim {
                return Ok(out);
            }
            out.push(c);
        }
        anyhow::bail!("unterminated expression: expected a closing {delim:?}")
    }

    fn read_text(&mut self) -> String {
        // `a text` (GNU) and `a\` + newline (BSD/POSIX) both reach here; the
        // leading backslash and blanks are skipped either way, so a script
        // written for either host does the same thing.
        while matches!(self.src.peek(), Some(' ' | '\t' | '\\' | '\n')) {
            self.src.next();
        }
        let mut out = String::new();
        for c in self.src.by_ref() {
            if c == '\n' {
                break;
            }
            out.push(c);
        }
        out
    }

    fn parse(&mut self) -> anyhow::Result<Vec<Command>> {
        let mut cmds = Vec::new();
        loop {
            self.skip_blank();
            if self.src.peek().is_none() {
                return Ok(cmds);
            }
            if self.src.peek() == Some(&'#') {
                for c in self.src.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }

            let first = self.parse_addr()?;
            let selector = match first {
                None => Selector::All,
                Some(a) => {
                    if self.src.peek() == Some(&',') {
                        self.src.next();
                        let second = self
                            .parse_addr()?
                            .ok_or_else(|| anyhow::anyhow!("expected an address after ','"))?;
                        Selector::Range(a, second)
                    } else {
                        Selector::One(a)
                    }
                }
            };
            while matches!(self.src.peek(), Some(' ' | '\t')) {
                self.src.next();
            }
            let negated = if self.src.peek() == Some(&'!') {
                self.src.next();
                true
            } else {
                false
            };
            while matches!(self.src.peek(), Some(' ' | '\t')) {
                self.src.next();
            }

            let Some(c) = self.src.next() else {
                anyhow::bail!("expected a command after an address");
            };
            let kind = match c {
                's' => {
                    let delim = self
                        .src
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("`s` needs a delimiter"))?;
                    let pat = self.read_until(delim)?;
                    let repl = self.read_until(delim)?;
                    let (mut global, mut print, mut icase, mut nth) = (false, false, false, 0usize);
                    while let Some(f) = self.src.peek().copied() {
                        match f {
                            'g' => global = true,
                            'p' => print = true,
                            'i' | 'I' => icase = true,
                            '0'..='9' => {
                                let mut n = String::new();
                                while let Some(d) = self.src.peek().copied() {
                                    if d.is_ascii_digit() {
                                        n.push(d);
                                        self.src.next();
                                    } else {
                                        break;
                                    }
                                }
                                nth = n.parse().unwrap_or(0);
                                continue;
                            }
                            _ => break,
                        }
                        self.src.next();
                    }
                    Kind::Substitute {
                        re: compile(&pat, self.extended, icase)?,
                        replacement: repl,
                        global,
                        nth,
                        print,
                    }
                }
                'y' => {
                    let delim = self
                        .src
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("`y` needs a delimiter"))?;
                    let from: Vec<char> = self.read_until(delim)?.chars().collect();
                    let to: Vec<char> = self.read_until(delim)?.chars().collect();
                    if from.len() != to.len() {
                        anyhow::bail!(
                            "`y` needs both sides the same length ({} vs {})",
                            from.len(),
                            to.len()
                        );
                    }
                    Kind::Transliterate(from, to)
                }
                'd' => Kind::Delete,
                'p' => Kind::Print,
                'q' => Kind::Quit,
                '=' => Kind::LineNumber,
                'a' => Kind::Append(self.read_text()),
                'i' => Kind::Insert(self.read_text()),
                'c' => Kind::Change(self.read_text()),
                '{' => anyhow::bail!(
                    "command groups `{{ … }}` are not supported — write each command with its \
                     own address instead"
                ),
                other => anyhow::bail!(
                    "unknown command {other:?} — heph's sed supports s, y, d, p, q, =, a, i and c"
                ),
            };
            cmds.push(Command {
                selector,
                negated,
                kind,
                in_range: std::cell::Cell::new(false),
            });
        }
    }
}

/// Expand `&` and `\1`..`\9` in a replacement, plus `\n`/`\t` and `\&`.
fn expand(replacement: &str, caps: &regex::Captures<'_>) -> String {
    let mut out = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '&' => out.push_str(caps.get(0).map_or("", |m| m.as_str())),
            '\\' => match chars.next() {
                Some(d @ '1'..='9') => {
                    let i = d.to_digit(10).unwrap_or(0) as usize;
                    out.push_str(caps.get(i).map_or("", |m| m.as_str()));
                }
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('&') => out.push('&'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out
}

fn substitute(re: &Regex, line: &str, repl: &str, global: bool, nth: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last = 0usize;
    let mut n = 0usize;
    for caps in re.captures_iter(line) {
        let Some(m) = caps.get(0) else { continue };
        n += 1;
        let wanted = if nth > 0 {
            n == nth || (global && n >= nth)
        } else {
            global || n == 1
        };
        if !wanted {
            continue;
        }
        out.push_str(line.get(last..m.start()).unwrap_or(""));
        out.push_str(&expand(repl, &caps));
        last = m.end();
        if !global && nth == 0 {
            break;
        }
    }
    out.push_str(line.get(last..).unwrap_or(""));
    out
}

fn selected(cmd: &Command, line: &str, lineno: u64, last_line: bool) -> bool {
    let hit = |a: &Addr| match a {
        Addr::Line(n) => *n == lineno,
        Addr::Last => last_line,
        Addr::Regex(re) => re.is_match(line),
    };
    let base = match &cmd.selector {
        Selector::All => true,
        Selector::One(a) => hit(a),
        Selector::Range(start, end) => {
            if cmd.in_range.get() {
                if hit(end) {
                    cmd.in_range.set(false);
                }
                true
            } else if hit(start) {
                // A single-line range (`/x/,/x/`) closes on a later line, which
                // is GNU's behaviour: the end address is looked for *after* the
                // start, never on the same line.
                cmd.in_range.set(true);
                true
            } else {
                false
            }
        }
    };
    base != cmd.negated
}

#[derive(Debug, Default)]
struct Options {
    quiet: bool,
    in_place: Option<String>,
    extended: bool,
    scripts: Vec<String>,
    files: Vec<PathBuf>,
}

fn parse_args(argv: &[OsString]) -> anyhow::Result<Options> {
    let mut o = Options::default();
    let mut script_given = false;
    let mut it = argv.iter().skip(1).peekable();
    let mut only_operands = false;

    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        if only_operands || !arg.starts_with('-') || arg == "-" {
            if script_given {
                o.files.push(PathBuf::from(raw));
            } else {
                o.scripts.push(arg);
                script_given = true;
            }
            continue;
        }
        if arg == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "quiet" | "silent" => o.quiet = true,
                "regexp-extended" => o.extended = true,
                "in-place" => o.in_place = Some(String::new()),
                other if other.starts_with("in-place=") => {
                    o.in_place = Some(other.trim_start_matches("in-place=").to_string());
                }
                other => anyhow::bail!("unrecognized option '--{other}'"),
            }
            continue;
        }
        let mut chars = arg.chars().skip(1).peekable();
        while let Some(c) = chars.next() {
            match c {
                'n' => o.quiet = true,
                'E' | 'r' => o.extended = true,
                's' => {}
                'i' => {
                    // The headline divergence: GNU takes an *optional* attached
                    // suffix, BSD requires a separate one. Taking only the
                    // attached form means `sed -i 's/a/b/' f` works, and
                    // `sed -i '' 's/a/b/' f` (the BSD spelling) leaves `''` as
                    // the script — which is why that form is rejected below
                    // rather than silently doing nothing.
                    let rest: String = chars.by_ref().collect();
                    o.in_place = Some(rest);
                }
                'e' => {
                    let rest: String = chars.by_ref().collect();
                    let value = if rest.is_empty() {
                        it.next().map(|v| v.to_string_lossy().into_owned())
                    } else {
                        Some(rest)
                    };
                    match value {
                        Some(v) => {
                            o.scripts.push(v);
                            script_given = true;
                        }
                        None => anyhow::bail!("option requires an argument -- e"),
                    }
                }
                'f' => {
                    let rest: String = chars.by_ref().collect();
                    let value = if rest.is_empty() {
                        it.next().map(|v| v.to_string_lossy().into_owned())
                    } else {
                        Some(rest)
                    };
                    match value {
                        Some(path) => {
                            o.scripts.push(std::fs::read_to_string(&path).map_err(|e| {
                                anyhow::anyhow!("cannot read script file {path}: {e}")
                            })?);
                            script_given = true;
                        }
                        None => anyhow::bail!("option requires an argument -- f"),
                    }
                }
                other => anyhow::bail!("invalid option -- {other}"),
            }
        }
    }

    if o.scripts.is_empty() {
        anyhow::bail!("no script given");
    }
    if o.in_place.is_some() && o.scripts.iter().any(String::is_empty) {
        anyhow::bail!(
            "an empty script — this looks like BSD's `sed -i '' 's/…/…/' file`. heph's sed \
             takes GNU's form: `sed -i 's/…/…/' file`, or `sed -i.bak` to keep a backup."
        );
    }
    Ok(o)
}

/// Run `cmds` over `input`, writing to `out`. Returns whether `q` fired.
fn run_stream(
    cmds: &[Command],
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    quiet: bool,
) -> anyhow::Result<bool> {
    let lines: Vec<String> = input.lines().collect::<std::io::Result<_>>()?;
    let total = lines.len();
    for cmd in cmds {
        cmd.in_range.set(false);
    }

    for (i, line) in lines.iter().enumerate() {
        let lineno = (i + 1) as u64;
        let last = i + 1 == total;
        let mut current = line.clone();
        let mut deleted = false;
        let mut quit = false;
        let mut appended: Vec<&str> = Vec::new();

        for cmd in cmds {
            if !selected(cmd, &current, lineno, last) {
                continue;
            }
            match &cmd.kind {
                Kind::Substitute {
                    re,
                    replacement,
                    global,
                    nth,
                    print,
                } => {
                    let before = current.clone();
                    current = substitute(re, &current, replacement, *global, *nth);
                    if *print && current != before {
                        writeln!(out, "{current}")?;
                    }
                }
                Kind::Delete => {
                    deleted = true;
                    break;
                }
                Kind::Print => writeln!(out, "{current}")?,
                Kind::Quit => {
                    quit = true;
                    break;
                }
                Kind::Transliterate(from, to) => {
                    current = current
                        .chars()
                        .map(|c| {
                            from.iter()
                                .position(|f| *f == c)
                                .and_then(|i| to.get(i).copied())
                                .unwrap_or(c)
                        })
                        .collect();
                }
                Kind::LineNumber => writeln!(out, "{lineno}")?,
                Kind::Append(text) => appended.push(text),
                Kind::Insert(text) => writeln!(out, "{text}")?,
                Kind::Change(text) => {
                    writeln!(out, "{text}")?;
                    deleted = true;
                    break;
                }
            }
        }

        if !deleted && !quiet {
            writeln!(out, "{current}")?;
        }
        for text in appended {
            writeln!(out, "{text}")?;
        }
        if quit {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn main(argv: Vec<OsString>) -> i32 {
    let program = argv
        .first()
        .and_then(|a| a.to_str())
        .unwrap_or("sed")
        .to_string();
    let o = match parse_args(&argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{program}: {e}");
            return ERROR;
        }
    };
    let script = o.scripts.join("\n");
    let cmds = match Parser::new(&script, o.extended).parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{program}: {e}");
            return ERROR;
        }
    };

    if o.files.is_empty() {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut r = stdin.lock();
        let mut w = stdout.lock();
        return match run_stream(&cmds, &mut r, &mut w, o.quiet).and_then(|_| Ok(w.flush()?)) {
            Ok(()) => OK,
            Err(e) => {
                eprintln!("{program}: {e}");
                ERROR
            }
        };
    }

    for path in &o.files {
        let res = (|| -> anyhow::Result<()> {
            let file = std::fs::File::open(path)?;
            let mut reader = std::io::BufReader::new(file);
            if let Some(suffix) = &o.in_place {
                let mut buf: Vec<u8> = Vec::new();
                run_stream(&cmds, &mut reader, &mut buf, o.quiet)?;
                if !suffix.is_empty() {
                    let backup = PathBuf::from(format!("{}{suffix}", path.display()));
                    std::fs::copy(path, backup)?;
                }
                // Written through a temporary in the same directory and renamed:
                // a truncate-then-write loses the file if anything fails halfway,
                // and this is editing someone's source.
                let dir = path.parent().unwrap_or(std::path::Path::new("."));
                let tmp = tempfile_in(dir)?;
                std::fs::write(&tmp, &buf)?;
                std::fs::rename(&tmp, path)?;
            } else {
                let stdout = std::io::stdout();
                let mut w = stdout.lock();
                run_stream(&cmds, &mut reader, &mut w, o.quiet)?;
                w.flush()?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("{program}: {}: {e}", path.display());
            return ERROR;
        }
    }
    OK
}

/// A unique sibling path for the in-place rename.
fn tempfile_in(dir: &std::path::Path) -> std::io::Result<PathBuf> {
    let name = format!(".heph-sed-{}-{:p}", std::process::id(), &dir);
    Ok(dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(script: &str, input: &str) -> String {
        run_opts(script, input, false, false)
    }

    fn run_opts(script: &str, input: &str, quiet: bool, extended: bool) -> String {
        let cmds = Parser::new(script, extended).parse().expect("parse");
        let mut out: Vec<u8> = Vec::new();
        let mut r = std::io::Cursor::new(input.as_bytes());
        run_stream(&cmds, &mut r, &mut out, quiet).expect("run");
        String::from_utf8(out).expect("utf-8")
    }

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    // ---- BRE translation, the thing most scripts depend on ----

    #[test]
    fn bre_groups_translate_to_ere() {
        // `s/\(a\)\(b\)/\2\1/` is the single most common idiom in real scripts.
        assert_eq!(run(r"s/\(a\)\(b\)/\2\1/", "ab\n"), "ba\n");
    }

    #[test]
    fn bare_parens_are_literal_in_a_bre() {
        assert_eq!(bre_to_ere("(x)").expect("translate"), r"\(x\)");
        assert_eq!(run("s/(x)/y/", "(x)\n"), "y\n");
    }

    #[test]
    fn bracket_expressions_are_passed_through_untouched() {
        // Nothing inside `[...]` follows BRE or ERE escaping rules, so the
        // translator must not touch it.
        assert_eq!(bre_to_ere("[a+b]").expect("translate"), "[a+b]");
        assert_eq!(run("s/[a+b]/./g", "a+b\n"), "...\n");
    }

    #[test]
    fn a_closing_bracket_first_is_literal() {
        assert_eq!(bre_to_ere("[]a]").expect("translate"), "[]a]");
    }

    #[test]
    fn backreferences_in_a_pattern_are_refused_by_name() {
        // The documented gap. Silently matching something else would be worse
        // than failing.
        let err = bre_to_ere(r"\(a\)\1").expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("backreference"), "{msg}");
        assert!(msg.contains("no backreferences"), "{msg}");
    }

    #[test]
    fn backreferences_are_refused_in_extended_mode_too() {
        check_ere(r"(a)\1").expect_err("must refuse");
        check_ere(r"(a)b").expect("no backreference here");
    }

    // ---- substitution ----

    #[test]
    fn substitution_replaces_the_first_match_by_default() {
        assert_eq!(run("s/a/X/", "aaa\n"), "Xaa\n");
    }

    #[test]
    fn the_g_flag_replaces_every_match() {
        assert_eq!(run("s/a/X/g", "aaa\n"), "XXX\n");
    }

    #[test]
    fn a_numeric_flag_replaces_the_nth_match() {
        assert_eq!(run("s/a/X/2", "aaa\n"), "aXa\n");
    }

    #[test]
    fn ampersand_is_the_whole_match() {
        assert_eq!(run(r"s/b/[&]/", "abc\n"), "a[b]c\n");
        assert_eq!(run(r"s/b/\&/", "abc\n"), "a&c\n");
    }

    #[test]
    fn escapes_in_the_replacement_expand() {
        assert_eq!(run(r"s/,/\n/g", "a,b\n"), "a\nb\n");
    }

    #[test]
    fn an_alternate_delimiter_avoids_escaping_slashes() {
        // `s|/usr|/opt|` is why delimiters are configurable at all.
        assert_eq!(run("s|/usr|/opt|", "/usr/bin\n"), "/opt/bin\n");
    }

    // ---- addresses ----

    #[test]
    fn a_line_address_selects_one_line() {
        assert_eq!(run("2d", "a\nb\nc\n"), "a\nc\n");
    }

    #[test]
    fn dollar_selects_the_last_line() {
        assert_eq!(run("$d", "a\nb\nc\n"), "a\nb\n");
    }

    #[test]
    fn a_regex_address_selects_matching_lines() {
        assert_eq!(run("/b/d", "a\nb\nc\n"), "a\nc\n");
    }

    #[test]
    fn a_range_spans_from_start_to_end() {
        assert_eq!(run("2,3d", "a\nb\nc\nd\n"), "a\nd\n");
    }

    #[test]
    fn negation_inverts_an_address() {
        assert_eq!(run("2!d", "a\nb\nc\n"), "b\n");
    }

    // ---- other commands ----

    #[test]
    fn quiet_plus_p_prints_only_what_was_selected() {
        // `sed -n '/x/p'` is grep-by-another-name and extremely common.
        assert_eq!(run_opts("/b/p", "a\nb\nc\n", true, false), "b\n");
    }

    #[test]
    fn q_stops_reading() {
        assert_eq!(run("2q", "a\nb\nc\n"), "a\nb\n");
    }

    #[test]
    fn y_transliterates() {
        assert_eq!(run("y/abc/xyz/", "cab\n"), "zxy\n");
    }

    #[test]
    fn y_rejects_mismatched_lengths() {
        Parser::new("y/ab/xyz/", false)
            .parse()
            .expect_err("lengths must match");
    }

    #[test]
    fn a_and_i_place_text_around_the_line() {
        assert_eq!(run("2i inserted", "a\nb\n"), "a\ninserted\nb\n");
        assert_eq!(run("1a appended", "a\nb\n"), "a\nappended\nb\n");
    }

    #[test]
    fn bsd_style_a_backslash_is_accepted() {
        // BSD needs `a\` + newline; GNU takes `a text`. Both must work, since
        // the point is that one script runs on both hosts.
        assert_eq!(run("1a\\\ntext", "x\n"), "x\ntext\n");
    }

    #[test]
    fn c_replaces_the_line() {
        assert_eq!(run("2c new", "a\nb\nc\n"), "a\nnew\nc\n");
    }

    #[test]
    fn equals_prints_the_line_number() {
        assert_eq!(run_opts("=", "a\nb\n", true, false), "1\n2\n");
    }

    // ---- argument handling ----

    #[test]
    fn i_takes_an_attached_suffix_gnu_style() {
        let o = parse_args(&argv(&["sed", "-i", "s/a/b/", "f"])).expect("parse");
        assert_eq!(o.in_place, Some(String::new()));
        assert_eq!(o.scripts, vec!["s/a/b/"]);
        assert_eq!(o.files.len(), 1);

        let b = parse_args(&argv(&["sed", "-i.bak", "s/a/b/", "f"])).expect("parse");
        assert_eq!(b.in_place, Some(".bak".to_string()));
    }

    #[test]
    fn the_bsd_spelling_of_i_is_refused_with_the_fix() {
        // `sed -i '' 's/a/b/' f` would otherwise take `''` as the script and
        // silently do nothing — the exact failure this applet exists to remove.
        let err = parse_args(&argv(&["sed", "-i", "", "s/a/b/", "f"])).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("BSD"), "{msg}");
        assert!(
            msg.contains("sed -i 's/"),
            "the fix must be spelled out: {msg}"
        );
    }

    #[test]
    fn multiple_e_scripts_run_in_order() {
        let o = parse_args(&argv(&["sed", "-e", "s/a/b/", "-e", "s/b/c/", "f"])).expect("parse");
        assert_eq!(o.scripts, vec!["s/a/b/", "s/b/c/"]);
        assert_eq!(run(&o.scripts.join("\n"), "a\n"), "c\n");
    }

    #[test]
    fn command_groups_are_refused_rather_than_half_supported() {
        let err = Parser::new("/x/{p;d}", false)
            .parse()
            .expect_err("must refuse");
        assert!(format!("{err:#}").contains("command groups"), "{err:#}");
    }

    #[test]
    fn an_unknown_command_names_what_is_supported() {
        let err = Parser::new("Z", false).parse().expect_err("must refuse");
        assert!(format!("{err:#}").contains("supports s, y, d"), "{err:#}");
    }
}
