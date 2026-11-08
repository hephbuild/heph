//! The managed section compiled into a matcher: which paths are generated
//! territory (J1).
//!
//! Lines get git's meaning, case-sensitive on every target: they compile into
//! the `ignore` crate's `Gitignore` with `case_insensitive(false)`. Two places
//! where `ignore` and git differ are bridged here:
//!
//! - `ignore` (through `globset`) reads `{a,b}` as alternation; git reads it
//!   literally. Unescaped braces outside a character class are escaped before
//!   a line compiles.
//! - `matched_path_or_any_parents` tests the path itself before its parents, so
//!   a `!x` line could re-include a file under an excluded directory, which git
//!   never does (it does not descend into one). A point lookup here walks the
//!   ancestors top-down and stops at the first excluded directory, exactly as a
//!   walk that carries "this directory is territory" down does, so the two
//!   always agree.
//!
//! Paths are normalised root-relative bytes ([`super::normalize_rel`]); one
//! outside the root never reaches the matcher.

use crate::gitignore_section::{ParsedSection, SectionLine, parse_section_bytes};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::borrow::Cow;
use std::path::PathBuf;

use super::paths::rel_path;

/// A compiled managed section.
#[derive(Debug)]
pub struct Section {
    parsed: ParsedSection,
    matcher: Gitignore,
}

/// Which section line decided a path, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionMatch {
    /// No line matches: not territory.
    None,
    /// A line covers the path (index into [`Section::lines`]).
    Territory(usize),
    /// A `!` line re-includes it (index into [`Section::lines`]): not territory.
    Negated(usize),
    /// A walk entry under a directory that is territory: territory without a
    /// match call. A caller that needs the covering line asks for a point match.
    Inherited,
}

impl SectionMatch {
    pub fn is_territory(self) -> bool {
        matches!(self, Self::Territory(_) | Self::Inherited)
    }

    /// The deciding line, if known.
    pub fn line(self) -> Option<usize> {
        match self {
            Self::None | Self::Inherited => None,
            Self::Territory(i) | Self::Negated(i) => Some(i),
        }
    }
}

/// How a lookup tests the section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionProbe {
    /// A lone path (`file()`, `why`, the guard, a fold hit): its ancestors are
    /// tested top-down, then the path.
    Point,
    /// An entry of a walk that carries territory down: only the entry itself
    /// is tested, unless its parent is already territory.
    Walk { parent_territory: bool },
}

/// `line` with every unescaped `{` and `}` outside a character class escaped,
/// so `globset` reads braces literally, as git does.
fn literal_braces(line: &str) -> Cow<'_, str> {
    if !line.contains(['{', '}']) {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len() + 4);
    let mut chars = line.chars().peekable();
    let mut in_class = false;
    let mut class_start = false;
    while let Some(c) = chars.next() {
        if in_class {
            out.push(c);
            if class_start && (c == '!' || c == '^') {
                continue;
            }
            if c == ']' && !class_start {
                in_class = false;
            }
            class_start = false;
            continue;
        }
        match c {
            '\\' => {
                out.push(c);
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '[' => {
                out.push(c);
                in_class = true;
                class_start = true;
            }
            '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

impl Section {
    /// Parse and compile a whole `.gitignore`. Parse warnings are logged once,
    /// here; a line `ignore` cannot compile is skipped with a warning.
    pub fn compile(gitignore: &[u8]) -> Self {
        let parsed = parse_section_bytes(gitignore);
        for w in &parsed.warnings {
            tracing::warn!("{w}");
        }
        let mut b = GitignoreBuilder::new("");
        // Case-sensitive on every target, as git is by default on Linux; the
        // fold key handles case-insensitive filesystems separately.
        if let Err(e) = b.case_insensitive(false) {
            tracing::warn!(error = %e, "setting the managed section matcher case-sensitive");
        }
        for (i, line) in parsed.lines.iter().enumerate() {
            if line.is_comment() {
                continue;
            }
            // The index is carried as the glob's `from`, the only per-glob
            // field `ignore` hands back on a match; nothing displays it.
            let from = Some(PathBuf::from(i.to_string()));
            if let Err(e) = b.add_line(from, &literal_braces(&line.entry.pattern)) {
                tracing::warn!(
                    line = line.line_no,
                    pattern = %line.entry.pattern,
                    error = %e,
                    ".gitignore managed section line does not compile; skipped"
                );
            }
        }
        let matcher = b.build().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "compiling the managed .gitignore section; read as empty");
            Gitignore::empty()
        });
        Self { parsed, matcher }
    }

    /// A section with no lines (no `.gitignore`, or no markers in it).
    pub fn empty() -> Self {
        Self {
            parsed: ParsedSection::default(),
            matcher: Gitignore::empty(),
        }
    }

    /// The workspace has a (well-formed) section.
    pub fn is_present(&self) -> bool {
        self.parsed.present
    }

    pub fn lines(&self) -> &[SectionLine] {
        &self.parsed.lines
    }

    pub fn line(&self, i: usize) -> Option<&SectionLine> {
        self.parsed.lines.get(i)
    }

    pub fn warnings(&self) -> &[String] {
        &self.parsed.warnings
    }

    fn to_match(m: Match<&ignore::gitignore::Glob>) -> SectionMatch {
        let idx = |g: &ignore::gitignore::Glob| {
            g.from()
                .and_then(|p| p.to_str())
                .and_then(|s| s.parse::<usize>().ok())
        };
        match m {
            Match::None => SectionMatch::None,
            Match::Ignore(g) => idx(g).map_or(SectionMatch::None, SectionMatch::Territory),
            Match::Whitelist(g) => idx(g).map_or(SectionMatch::None, SectionMatch::Negated),
        }
    }

    /// Test one entry alone, ignoring its ancestors.
    fn match_one(&self, rel: &[u8], is_dir: bool) -> SectionMatch {
        if self.matcher.is_empty() || rel.is_empty() || rel.first() == Some(&b'/') {
            return SectionMatch::None;
        }
        Self::to_match(self.matcher.matched(rel_path(rel), is_dir))
    }

    /// Test a walk entry: territory if its parent is, else the entry alone.
    /// The walk carries the result down for a directory (see
    /// [`Self::dir_is_territory`]).
    pub fn match_walk(&self, rel: &[u8], is_dir: bool, parent_territory: bool) -> SectionMatch {
        if parent_territory {
            // git does not descend into an excluded directory, so nothing
            // under it can be re-included.
            return SectionMatch::Inherited;
        }
        self.match_one(rel, is_dir)
    }

    /// Whether a walk's directory `rel` is territory, given its parent's
    /// answer: every entry under it is then territory without a match call.
    pub fn dir_is_territory(&self, rel: &[u8], parent_territory: bool) -> bool {
        parent_territory || self.match_one(rel, true).is_territory()
    }

    /// Test a lone path: ancestors top-down, stopping at the first one that is
    /// territory (git does not descend into an excluded directory), then the
    /// path itself.
    pub fn match_point(&self, rel: &[u8], is_dir: bool) -> SectionMatch {
        if self.matcher.is_empty() || rel.is_empty() || rel.first() == Some(&b'/') {
            return SectionMatch::None;
        }
        for (i, &c) in rel.iter().enumerate() {
            if c != b'/' {
                continue;
            }
            let Some(ancestor) = rel.get(..i) else {
                continue;
            };
            let m = self.match_one(ancestor, true);
            if m.is_territory() {
                return m;
            }
        }
        self.match_one(rel, is_dir)
    }

    /// Test `rel` the way `probe` says.
    pub fn probe(&self, rel: &[u8], is_dir: bool, probe: SectionProbe) -> SectionMatch {
        match probe {
            SectionProbe::Point => self.match_point(rel, is_dir),
            SectionProbe::Walk { parent_territory } => {
                self.match_walk(rel, is_dir, parent_territory)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitignore_section::{BEGIN_MARKER, END_MARKER};

    fn section(lines: &[&str]) -> Section {
        let mut text = format!("{BEGIN_MARKER}\n");
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        text.push_str(END_MARKER);
        text.push('\n');
        Section::compile(text.as_bytes())
    }

    fn territory(s: &Section, p: &str, is_dir: bool) -> bool {
        s.match_point(p.as_bytes(), is_dir).is_territory()
    }

    #[test]
    fn malformed_sections_read_as_none() {
        let s = Section::compile(format!("/a\n{END_MARKER}\n").as_bytes());
        assert!(!s.is_present());
        assert!(!territory(&s, "a", false));
        let s = Section::compile(format!("{END_MARKER}\n{BEGIN_MARKER}\n/a\n").as_bytes());
        assert!(!s.is_present());
        assert!(!territory(&s, "a", false));
    }

    #[test]
    fn crlf_lines_match() {
        let text = format!("{BEGIN_MARKER}\r\n# //p:gen\r\n/p/a.go\r\n{END_MARKER}\r\n");
        let s = Section::compile(text.as_bytes());
        assert!(territory(&s, "p/a.go", false));
    }

    #[test]
    fn directory_line_covers_everything_under_it_and_the_symlink() {
        let s = section(&["# //p:gen", "/p/gen"]);
        assert!(territory(&s, "p/gen/x/y.go", false));
        // `p/gen` materialized as a symlink is a file to git.
        assert!(territory(&s, "p/gen", false));
        assert!(!territory(&s, "p/genx", false));
        assert!(!territory(&s, "q/p/gen", false));
    }

    #[test]
    fn trailing_slash_matches_directories_only() {
        let s = section(&["/p/gen/"]);
        assert!(territory(&s, "p/gen/x.go", false));
        assert!(territory(&s, "p/gen", true));
        assert!(!territory(&s, "p/gen", false), "the symlink is not a dir");
    }

    #[test]
    fn unanchored_line_matches_at_any_depth_and_is_foreign() {
        let s = section(&["*.pb.go"]);
        assert!(territory(&s, "a/b/c.pb.go", false));
        assert!(territory(&s, "c.pb.go", false));
        assert!(s.lines()[0].is_foreign());
    }

    #[test]
    fn negation_reincludes_like_git() {
        let s = section(&["/p/*.go", "!p/keep.go"]);
        assert!(territory(&s, "p/a.go", false));
        assert!(matches!(
            s.match_point(b"p/keep.go", false),
            SectionMatch::Negated(1)
        ));
        assert!(s.lines()[1].is_foreign());
        // Under an excluded directory, git never re-includes.
        let s = section(&["/p/gen", "!p/gen/keep.go"]);
        assert!(territory(&s, "p/gen/keep.go", false));
    }

    #[test]
    fn escaped_hash_is_a_literal() {
        let s = section(&["\\#foo"]);
        assert!(territory(&s, "#foo", false));
        assert!(!territory(&s, "foo", false));
    }

    #[test]
    fn brackets_and_stars_read_as_git_reads_them() {
        let s = section(&["/p/a[12].go", "/q/*.txt"]);
        assert!(territory(&s, "p/a1.go", false));
        assert!(!territory(&s, "p/a[12].go", false));
        assert!(territory(&s, "q/x.txt", false));
        assert!(!territory(&s, "q/sub/x.txt", false));
    }

    #[test]
    fn braces_are_literal() {
        let s = section(&["/p/{a,b}.go"]);
        assert!(territory(&s, "p/{a,b}.go", false));
        assert!(!territory(&s, "p/a.go", false));
        assert!(!territory(&s, "p/b.go", false));
        // An already-escaped brace stays one literal brace.
        let s = section(&["/p/\\{x\\}.go"]);
        assert!(territory(&s, "p/{x}.go", false));
        // Inside a class a brace is a member.
        let s = section(&["/p/[{]x.go"]);
        assert!(territory(&s, "p/{x.go", false));
    }

    #[test]
    fn matching_is_case_sensitive_on_every_target() {
        let s = section(&["/Gen"]);
        assert!(!territory(&s, "gen/x", false));
        assert!(territory(&s, "Gen/x", false));
    }

    #[test]
    fn unattributed_line_is_still_territory_and_foreign() {
        let s = section(&["/p/a.go"]);
        assert!(territory(&s, "p/a.go", false));
        assert!(s.lines()[0].is_foreign());
        assert_eq!(s.match_point(b"p/a.go", false).line(), Some(0));
    }

    #[test]
    fn outside_root_paths_never_match_and_never_panic() {
        let s = section(&["*"]);
        assert_eq!(s.match_point(b"/etc/passwd", false), SectionMatch::None);
        assert_eq!(s.match_point(b"", false), SectionMatch::None);
        assert_eq!(s.match_walk(b"/x", false, false), SectionMatch::None);
    }

    #[test]
    fn walk_carry_agrees_with_point_lookup() {
        let s = section(&["/p/gen", "!p/gen/keep.go", "/q/*.go"]);
        for file in ["p/gen/keep.go", "p/gen/x.go", "q/a.go", "r/a.go"] {
            let comps: Vec<&str> = file.split('/').collect();
            let mut terr = false;
            let mut prefix = String::new();
            for c in &comps[..comps.len() - 1] {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(c);
                terr = s.dir_is_territory(prefix.as_bytes(), terr);
            }
            let walk = s.match_walk(file.as_bytes(), false, terr).is_territory();
            assert_eq!(walk, territory(&s, file, false), "{file}");
        }
    }

    #[test]
    fn the_matched_line_is_reported() {
        let s = section(&["# //p:gen", "/p/a.go", "# //q:gen", "/q/b.go"]);
        let i = s.match_point(b"q/b.go", false).line().unwrap();
        let line = s.line(i).unwrap();
        assert_eq!(line.entry.pattern, "/q/b.go");
        assert_eq!(line.line_no, 5);
        assert_eq!(line.entry.addr.as_ref().unwrap().format(), "//q:gen");
    }
}
