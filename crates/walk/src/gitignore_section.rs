//! The heph-managed `.gitignore` section: its markers and its one parser.
//!
//! The section is written by the engine (`engine::gitignore::render`) and read
//! by two parties that must agree on its extent: the engine, which merges a
//! scoped rebuild into it, and the codegen index, which compiles its lines into
//! the "generated territory" matcher. Both go through [`parse_section_bytes`],
//! so there is exactly one parser. The engine re-exports this module.

use hmodel::htaddr::{Addr, parse_addr};

/// Markers delimiting the heph-managed region. Lines between them (inclusive)
/// are owned by heph and rewritten on every run; everything outside is
/// preserved verbatim.
pub const BEGIN_MARKER: &str =
    "# BEGIN heph-generated (managed by `heph tool gen-gitignore` — do not edit)";
pub const END_MARKER: &str = "# END heph-generated";

/// Stable prefix of [`BEGIN_MARKER`]. Detection matches on this rather than the
/// full marker so that changing the parenthetical (e.g. the command name) never
/// orphans an already-committed section — the old block is still found and
/// rewritten with the current [`BEGIN_MARKER`] text.
pub const BEGIN_MARKER_PREFIX: &str = "# BEGIN heph-generated";

/// Prefix marking an attribution comment line: `# //pkg:target`, rendered on its
/// own line *above* the pattern it annotates.
///
/// git only treats a `#` as a comment when it is the **first** character of the
/// line — a trailing `pattern # //pkg:target` is not a comment, it is the literal
/// pattern `pattern # //pkg:target` (space and hash included), which matches a
/// file of that exact name rather than `pattern`. So attribution must live on a
/// preceding full-line comment; it round-trips through [`parse_section`].
pub const COMMENT_PREFIX: &str = "# ";

/// One managed `.gitignore` line: a root-anchored pattern plus the target that
/// emits it. The target is rendered as a `# //pkg:target` comment on the line
/// *above* the pattern so a scoped rebuild can tell which lines it owns without
/// re-scanning the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitignoreEntry {
    pub pattern: String,
    /// The target whose `codegen = copy` output produces `pattern`. `None` only
    /// for lines parsed from an existing section that lack the comment (legacy
    /// or hand-edited) — kept verbatim so a scoped rebuild never drops them.
    pub addr: Option<Addr>,
}

impl GitignoreEntry {
    /// Render as gitignore text: a `# //pkg:target` attribution comment line
    /// followed by the pattern line, or just the bare pattern when the emitting
    /// target is unknown.
    pub fn to_line(&self) -> String {
        match &self.addr {
            Some(addr) => format!("{COMMENT_PREFIX}{}\n{}", addr.format(), self.pattern),
            None => self.pattern.clone(),
        }
    }

    /// Sort key: by pattern first, then by emitting target so lines are
    /// deterministic and dedupe-comparable.
    pub fn sort_key(&self) -> (&str, String) {
        (
            self.pattern.as_str(),
            self.addr.as_ref().map(Addr::format).unwrap_or_default(),
        )
    }
}

/// One line of a parsed section, with where it sits in the `.gitignore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionLine {
    /// 1-based line number of the pattern line in the whole `.gitignore`.
    pub line_no: usize,
    pub entry: GitignoreEntry,
}

impl SectionLine {
    /// A comment line kept verbatim (a dangling attribution, or a `#` line that
    /// names no target). git reads it as a comment, so it matches nothing.
    pub fn is_comment(&self) -> bool {
        self.entry.pattern.starts_with('#')
    }

    /// A line heph did not write: no attribution comment, or a shape
    /// `content_to_pattern` never emits (it always emits a root-anchored `/…`,
    /// so `*.pb.go` and `!x` are foreign). Still territory; listed by
    /// `codegen status` and named by `why`, never an error.
    pub fn is_foreign(&self) -> bool {
        self.entry.addr.is_none() || !self.entry.pattern.starts_with('/')
    }
}

/// A parse of a whole `.gitignore`'s managed section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSection {
    /// Both markers were found, BEGIN before END. A `.gitignore` without a
    /// section, or with a malformed one, has none.
    pub present: bool,
    /// The section's lines, in file order (BEGIN line excluded).
    pub lines: Vec<SectionLine>,
    /// What the parse noticed and recovered from: a malformed section (read as
    /// none), a second BEGIN, a non-UTF-8 line (skipped). The caller decides
    /// how to surface them.
    pub warnings: Vec<String>,
}

/// Find `needle` in `hay`, as `str::find` does on text.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse the managed section out of a whole `.gitignore`.
///
/// The extent is the first [`BEGIN_MARKER_PREFIX`] to the first [`END_MARKER`].
/// END missing, BEGIN missing while END is present, or END before BEGIN is a
/// malformed section: no section, with a warning. Lines are trimmed (so CRLF and
/// surrounding blanks do not matter) and blank ones skipped. A `# //pkg:target`
/// line attributes the next pattern line; any other `#` line, and an attribution
/// left dangling, is kept verbatim as an unattributed comment entry so a scoped
/// rebuild never drops it.
pub fn parse_section_bytes(existing: &[u8]) -> ParsedSection {
    let begin = find_bytes(existing, BEGIN_MARKER_PREFIX.as_bytes());
    let end = find_bytes(existing, END_MARKER.as_bytes());
    let mut out = ParsedSection::default();
    let (start, end) = match (begin, end) {
        (None, None) => return out,
        (Some(start), Some(end)) if end >= start => (start, end),
        (Some(_), Some(_)) => {
            out.warnings
                .push("managed .gitignore section is malformed: its END marker comes before its BEGIN marker; read as no section".to_string());
            return out;
        }
        (None, Some(_)) => {
            out.warnings.push(
                "managed .gitignore section is malformed: END marker without a BEGIN marker; read as no section"
                    .to_string(),
            );
            return out;
        }
        (Some(_), None) => {
            out.warnings.push(
                "managed .gitignore section is malformed: BEGIN marker without an END marker; read as no section"
                    .to_string(),
            );
            return out;
        }
    };
    out.present = true;
    let Some(body) = existing.get(start..end) else {
        return out;
    };
    let first_line_no = existing
        .get(..start)
        .map_or(0, |b| b.iter().filter(|&&c| c == b'\n').count())
        + 1;

    let mut pending: Option<(Addr, usize)> = None;
    let mut begins = 0usize;
    for (i, raw) in body.split(|&c| c == b'\n').enumerate() {
        let line_no = first_line_no + i;
        let Ok(line) = std::str::from_utf8(raw) else {
            out.warnings.push(format!(
                ".gitignore line {line_no} in the managed section is not UTF-8; skipped"
            ));
            continue;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with(BEGIN_MARKER_PREFIX) {
            begins += 1;
            if begins == 2 {
                out.warnings.push(format!(
                    ".gitignore line {line_no}: a second BEGIN marker inside the managed section; \
                     the section runs from the first BEGIN to the first END"
                ));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            // A `#` line is an attribution comment if the remainder parses as a
            // target addr; flush any dangling pending comment as a bare line
            // first so nothing is silently dropped, then hold this one.
            if let Ok(addr) = parse_addr(rest.trim()) {
                if let Some((prev, prev_no)) = pending.take() {
                    out.lines.push(SectionLine {
                        line_no: prev_no,
                        entry: GitignoreEntry {
                            pattern: format!("{COMMENT_PREFIX}{}", prev.format()),
                            addr: None,
                        },
                    });
                }
                pending = Some((addr, line_no));
            } else {
                out.lines.push(SectionLine {
                    line_no,
                    entry: GitignoreEntry {
                        pattern: line.to_string(),
                        addr: None,
                    },
                });
            }
            continue;
        }
        out.lines.push(SectionLine {
            line_no,
            entry: GitignoreEntry {
                pattern: line.to_string(),
                addr: pending.take().map(|(addr, _)| addr),
            },
        });
    }
    // A trailing attribution comment with no following pattern is preserved.
    if let Some((prev, prev_no)) = pending {
        out.lines.push(SectionLine {
            line_no: prev_no,
            entry: GitignoreEntry {
                pattern: format!("{COMMENT_PREFIX}{}", prev.format()),
                addr: None,
            },
        });
    }
    out
}

/// The managed section's entries, as the engine's merge and render read them.
/// [`parse_section_bytes`] without the positions and warnings.
pub fn parse_section(existing: &str) -> Vec<GitignoreEntry> {
    parse_section_bytes(existing.as_bytes())
        .lines
        .into_iter()
        .map(|l| l.entry)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(p: &ParsedSection) -> Vec<&str> {
        p.lines.iter().map(|l| l.entry.pattern.as_str()).collect()
    }

    #[test]
    fn end_without_begin_and_end_before_begin_are_no_section_with_a_warning() {
        let p = parse_section_bytes(format!("/a\n{END_MARKER}\n").as_bytes());
        assert!(!p.present);
        assert!(p.lines.is_empty());
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);

        let p = parse_section_bytes(format!("{END_MARKER}\n/a\n{BEGIN_MARKER}\n").as_bytes());
        assert!(!p.present);
        assert!(p.lines.is_empty());
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
    }

    #[test]
    fn two_begins_use_first_begin_to_first_end_with_a_warning() {
        let text = format!("{BEGIN_MARKER}\n/a\n{BEGIN_MARKER}\n/b\n{END_MARKER}\n/c\n");
        let p = parse_section_bytes(text.as_bytes());
        assert!(p.present);
        assert_eq!(patterns(&p), vec!["/a", "/b"]);
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
    }

    #[test]
    fn crlf_line_endings_are_stripped() {
        let text = format!("{BEGIN_MARKER}\r\n# //p:gen\r\n/p/a.go\r\n{END_MARKER}\r\n");
        let p = parse_section_bytes(text.as_bytes());
        assert_eq!(patterns(&p), vec!["/p/a.go"]);
        assert!(p.lines[0].entry.addr.is_some());
    }

    #[test]
    fn a_bom_before_the_section_still_finds_the_markers() {
        let text = format!("\u{feff}{BEGIN_MARKER}\n/a\n{END_MARKER}\n");
        let p = parse_section_bytes(text.as_bytes());
        assert!(p.present);
        assert_eq!(patterns(&p), vec!["/a"]);
    }

    #[test]
    fn a_non_utf8_line_is_skipped_with_a_warning() {
        let mut text = format!("{BEGIN_MARKER}\n/a\n").into_bytes();
        text.extend_from_slice(b"/b\xff\n");
        text.extend_from_slice(format!("/c\n{END_MARKER}\n").as_bytes());
        let p = parse_section_bytes(&text);
        assert_eq!(patterns(&p), vec!["/a", "/c"]);
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
    }

    #[test]
    fn a_legacy_begin_text_is_found_by_its_prefix() {
        let text = format!("# BEGIN heph-generated (an old parenthetical)\n/a\n{END_MARKER}\n");
        let p = parse_section_bytes(text.as_bytes());
        assert!(p.present);
        assert_eq!(patterns(&p), vec!["/a"]);
    }

    #[test]
    fn line_numbers_count_the_whole_file() {
        let text = format!("node_modules\n\n{BEGIN_MARKER}\n# //p:gen\n/p/a.go\n{END_MARKER}\n");
        let p = parse_section_bytes(text.as_bytes());
        assert_eq!(p.lines[0].line_no, 5);
        assert!(!p.lines[0].is_foreign());
    }

    #[test]
    fn foreign_lines_are_unattributed_or_unanchored() {
        let text = format!(
            "{BEGIN_MARKER}\n/bare\n# //p:gen\n*.pb.go\n# //p:gen\n!p/keep.go\n# //p:gen\n/p/a.go\n{END_MARKER}\n"
        );
        let p = parse_section_bytes(text.as_bytes());
        let foreign: Vec<bool> = p.lines.iter().map(SectionLine::is_foreign).collect();
        assert_eq!(foreign, vec![true, true, true, false]);
    }
}
