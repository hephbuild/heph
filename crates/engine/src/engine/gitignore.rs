//! Generation of the heph-managed `.gitignore` section.
//!
//! Codegen targets with `codegen = copy` write generated files into the source
//! tree; those files must be gitignored. This module owns the *core* logic —
//! enumerating the patterns and rendering the marked section — so it can be
//! reused by the `tool gen-gitignore` command and by future validations (e.g. a
//! CI check that the committed `.gitignore` is up to date). The command layer
//! only handles file IO and progress reporting.

use std::sync::Arc;

use futures::TryStreamExt;

use crate::engine::Engine;
use crate::engine::driver::targetdef::path::{CodegenMode, Content};
use crate::engine::request_state::RequestState;
use hmodel::htaddr::{Addr, parse_addr};
use hmodel::htmatcher::{MatchResult, Matcher};

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

/// Separator between a gitignore pattern and the trailing `# //pkg:target`
/// comment naming the emitting target. The space-hash-space form keeps the
/// comment valid gitignore syntax and round-trips through [`parse_entry`].
const COMMENT_SEP: &str = " # ";

/// One managed `.gitignore` line: a root-anchored pattern plus the target that
/// emits it. The target is rendered as a trailing `# //pkg:target` comment so a
/// scoped rebuild can tell which lines it owns without re-scanning the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitignoreEntry {
    pub pattern: String,
    /// The target whose `codegen = copy` output produces `pattern`. `None` only
    /// for lines parsed from an existing section that lack the comment (legacy
    /// or hand-edited) — kept verbatim so a scoped rebuild never drops them.
    pub addr: Option<Addr>,
}

impl GitignoreEntry {
    /// Render as a single gitignore line: `pattern # //pkg:target`, or just the
    /// bare pattern when the emitting target is unknown.
    fn to_line(&self) -> String {
        match &self.addr {
            Some(addr) => format!("{}{COMMENT_SEP}{}", self.pattern, addr.format()),
            None => self.pattern.clone(),
        }
    }

    /// Sort key: by pattern first, then by emitting target so lines are
    /// deterministic and dedupe-comparable.
    fn sort_key(&self) -> (&str, String) {
        (
            self.pattern.as_str(),
            self.addr.as_ref().map(Addr::format).unwrap_or_default(),
        )
    }
}

/// Sort, then drop exact `(pattern, addr)` duplicates so the rendered section is
/// deterministic. Two *different* targets emitting the same path stay as two
/// distinct lines (that collision is a `validate` error, not a gitignore one).
fn normalize(mut entries: Vec<GitignoreEntry>) -> Vec<GitignoreEntry> {
    entries.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    entries.dedup();
    entries
}

impl Engine {
    /// Enumerate the root-anchored `.gitignore` entries for every
    /// `codegen = copy` output produced by a target matching `matcher`. Sorted
    /// and deduplicated so the result is deterministic and directly diffable
    /// against an existing file.
    ///
    /// Whole-workspace callers pass [`Matcher::TreeOutputTo`] with an empty
    /// package (reaches every codegen target); scoped callers pass the user's
    /// package matcher so only that slice of the dependency graph is walked.
    pub async fn codegen_copy_gitignore_patterns(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
    ) -> anyhow::Result<Vec<GitignoreEntry>> {
        let mut entries: Vec<GitignoreEntry> = Vec::new();
        let stream = Arc::clone(&self).query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            let def = Arc::clone(&self).get_def(rs.clone(), &addr).await?;
            for output in &def.target_def.outputs {
                for path in &output.paths {
                    if path.codegen_tree == CodegenMode::Copy {
                        entries.push(GitignoreEntry {
                            pattern: content_to_pattern(&path.content),
                            addr: Some(addr.clone()),
                        });
                    }
                }
            }
        }
        Ok(normalize(entries))
    }
}

/// Parse a single managed-section line into a [`GitignoreEntry`]. Splits on the
/// `# //pkg:target` comment and parses the target; a line without a parseable
/// comment becomes an entry with `addr = None` (preserved verbatim on rebuild).
fn parse_entry(line: &str) -> GitignoreEntry {
    if let Some((pattern, comment)) = line.split_once(COMMENT_SEP)
        && let Ok(addr) = parse_addr(comment.trim())
    {
        return GitignoreEntry {
            pattern: pattern.to_string(),
            addr: Some(addr),
        };
    }
    GitignoreEntry {
        pattern: line.to_string(),
        addr: None,
    }
}

/// Extract the entries currently inside the heph-managed marker section of
/// `existing`. Returns an empty vec when no section is present. Marker lines and
/// blank lines are skipped; every other line is parsed via [`parse_entry`].
#[expect(
    clippy::string_slice,
    reason = "slice indices come from `find` on ASCII markers — always char-aligned"
)]
pub fn parse_section(existing: &str) -> Vec<GitignoreEntry> {
    let (Some(start), Some(end)) = (
        existing.find(BEGIN_MARKER_PREFIX),
        existing.find(END_MARKER),
    ) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    existing[start..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(BEGIN_MARKER_PREFIX))
        .map(parse_entry)
        .collect()
}

/// Merge a scoped rebuild into the existing section. Existing entries whose
/// emitting target matches `matcher` are dropped (regenerated by `fresh`);
/// everything else — other packages' entries and un-attributed legacy lines —
/// is preserved. The union is sorted and deduplicated.
///
/// Only valid for decisive package matchers (`Package` / `PackagePrefix`), which
/// resolve `matches_addr` without a def. The whole-workspace path must *not* go
/// through here — it replaces the section wholesale instead.
pub fn merge_section(
    existing: &str,
    fresh: Vec<GitignoreEntry>,
    matcher: &Matcher,
) -> Vec<GitignoreEntry> {
    let mut merged: Vec<GitignoreEntry> = parse_section(existing)
        .into_iter()
        .filter(|e| match &e.addr {
            Some(addr) => matcher.matches_addr(addr) != MatchResult::MatchYes,
            None => true,
        })
        .collect();
    merged.extend(fresh);
    normalize(merged)
}

/// Convert an output path into a root-anchored `.gitignore` pattern. Output
/// paths are already workspace-root-relative (package-rooted), so a leading
/// `/` anchors them precisely.
///
/// Reused by `validate` as the canonical, normalized output-path key for
/// detecting overlapping `codegen = copy` outputs across targets.
pub(crate) fn content_to_pattern(content: &Content) -> String {
    match content {
        Content::FilePath(p) => format!("/{}", p.trim_start_matches('/')),
        Content::DirPath(p) => format!("/{}/", p.trim_start_matches('/').trim_end_matches('/')),
        Content::Glob(g) => format!("/{}", g.trim_start_matches('/')),
    }
}

/// Render the new `.gitignore` content: replace the heph-managed marker section
/// in `existing` with `entries`, or append a fresh section if no markers are
/// present. Content outside the markers is preserved verbatim. Idempotent.
#[expect(
    clippy::string_slice,
    reason = "slice indices come from `find` on ASCII markers — always char-aligned"
)]
pub fn render(existing: &str, entries: &[GitignoreEntry]) -> String {
    let mut section = String::new();
    section.push_str(BEGIN_MARKER);
    section.push('\n');
    for e in entries {
        section.push_str(&e.to_line());
        section.push('\n');
    }
    section.push_str(END_MARKER);

    match (
        existing.find(BEGIN_MARKER_PREFIX),
        existing.find(END_MARKER),
    ) {
        (Some(start), Some(end_marker_pos)) if end_marker_pos >= start => {
            let end = end_marker_pos + END_MARKER.len();
            let mut result = String::with_capacity(existing.len() + section.len());
            result.push_str(&existing[..start]);
            result.push_str(&section);
            result.push_str(&existing[end..]);
            result
        }
        _ => {
            let mut result = String::with_capacity(existing.len() + section.len() + 2);
            result.push_str(existing);
            if !existing.is_empty() && !existing.ends_with('\n') {
                result.push('\n');
            }
            if !existing.is_empty() {
                result.push('\n');
            }
            result.push_str(&section);
            result.push('\n');
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmodel::htpkg::PkgBuf;

    /// Bare entry (no emitting target) — renders as just the pattern.
    fn bare(pattern: &str) -> GitignoreEntry {
        GitignoreEntry {
            pattern: pattern.to_string(),
            addr: None,
        }
    }

    /// Entry attributed to `addr` (e.g. `//pkg:target`).
    fn attributed(pattern: &str, addr: &str) -> GitignoreEntry {
        GitignoreEntry {
            pattern: pattern.to_string(),
            addr: Some(parse_addr(addr).expect("valid addr")),
        }
    }

    #[test]
    fn content_to_pattern_anchors_paths() {
        assert_eq!(
            content_to_pattern(&Content::FilePath("foo/bar.go".into())),
            "/foo/bar.go"
        );
        assert_eq!(
            content_to_pattern(&Content::DirPath("foo/gen".into())),
            "/foo/gen/"
        );
        assert_eq!(
            content_to_pattern(&Content::Glob("foo/gen/**/*.go".into())),
            "/foo/gen/**/*.go"
        );
    }

    #[test]
    fn render_appends_section_to_empty_file() {
        let out = render("", &[bare("/a"), bare("/b")]);
        assert_eq!(out, format!("{BEGIN_MARKER}\n/a\n/b\n{END_MARKER}\n"));
    }

    #[test]
    fn render_emits_target_comment() {
        let out = render("", &[attributed("/foo/gen.go", "//foo:gen")]);
        assert_eq!(
            out,
            format!("{BEGIN_MARKER}\n/foo/gen.go # //foo:gen\n{END_MARKER}\n")
        );
    }

    #[test]
    fn render_appends_after_existing_content_with_blank_line() {
        let out = render("node_modules\n", &[bare("/gen/")]);
        assert_eq!(
            out,
            format!("node_modules\n\n{BEGIN_MARKER}\n/gen/\n{END_MARKER}\n")
        );
    }

    #[test]
    fn render_inserts_newline_when_existing_lacks_trailing() {
        let out = render("node_modules", &[bare("/gen/")]);
        assert_eq!(
            out,
            format!("node_modules\n\n{BEGIN_MARKER}\n/gen/\n{END_MARKER}\n")
        );
    }

    #[test]
    fn render_replaces_existing_section_preserving_surroundings() {
        let existing = format!("top\n{BEGIN_MARKER}\n/old\n{END_MARKER}\nbottom\n");
        let out = render(&existing, &[bare("/new")]);
        assert_eq!(
            out,
            format!("top\n{BEGIN_MARKER}\n/new\n{END_MARKER}\nbottom\n")
        );
    }

    #[test]
    fn detects_and_rewrites_legacy_begin_marker() {
        // A section committed before the command moved to `heph tool gen-gitignore`
        // carries the old parenthetical. Detection keys on the stable prefix, so the
        // block is found, parsed, and rewritten with the current marker text — never
        // orphaned into a duplicate.
        let legacy = "# BEGIN heph-generated (managed by `heph gen-gitignore` — do not edit)\n/old\n# END heph-generated\n";
        assert_eq!(parse_section(legacy), vec![bare("/old")]);

        let out = render(legacy, &[bare("/new")]);
        assert_eq!(out, format!("{BEGIN_MARKER}\n/new\n{END_MARKER}\n"));
    }

    #[test]
    fn render_shrinks_section_when_entries_removed() {
        let existing = format!("{BEGIN_MARKER}\n/a\n/b\n/c\n{END_MARKER}\n");
        let out = render(&existing, &[bare("/a")]);
        assert_eq!(out, format!("{BEGIN_MARKER}\n/a\n{END_MARKER}\n"));
    }

    #[test]
    fn render_empties_section_when_no_entries() {
        let existing = format!("keep\n{BEGIN_MARKER}\n/a\n{END_MARKER}\n");
        let out = render(&existing, &[]);
        assert_eq!(out, format!("keep\n{BEGIN_MARKER}\n{END_MARKER}\n"));
    }

    #[test]
    fn render_is_idempotent() {
        let first = render("keep\n", &[bare("/a"), bare("/b")]);
        let second = render(&first, &[bare("/a"), bare("/b")]);
        assert_eq!(first, second);
    }

    #[test]
    fn parse_section_round_trips_attributed_lines() {
        let rendered = render(
            "",
            &[
                attributed("/foo/a.go", "//foo:gen"),
                attributed("/bar/b.go", "//bar:gen"),
            ],
        );
        let parsed = parse_section(&rendered);
        assert_eq!(
            parsed,
            vec![
                attributed("/foo/a.go", "//foo:gen"),
                attributed("/bar/b.go", "//bar:gen"),
            ]
        );
    }

    #[test]
    fn parse_section_keeps_bare_lines_unattributed() {
        let existing = format!("{BEGIN_MARKER}\n/legacy\n{END_MARKER}\n");
        assert_eq!(parse_section(&existing), vec![bare("/legacy")]);
    }

    #[test]
    fn parse_section_empty_without_markers() {
        assert!(parse_section("node_modules\n").is_empty());
    }

    #[test]
    fn merge_section_replaces_only_in_scope_targets() {
        // Existing section: one foo entry, one bar entry. A scoped rebuild of
        // `//foo/...` drops foo's line and substitutes the freshly-scanned one,
        // leaving bar's untouched.
        let existing = render(
            "",
            &[
                attributed("/foo/old.go", "//foo:gen"),
                attributed("/bar/keep.go", "//bar:gen"),
            ],
        );
        let fresh = vec![attributed("/foo/new.go", "//foo:gen")];
        let matcher = Matcher::PackagePrefix(PkgBuf::from("foo"));

        let merged = merge_section(&existing, fresh, &matcher);
        assert_eq!(
            merged,
            vec![
                attributed("/bar/keep.go", "//bar:gen"),
                attributed("/foo/new.go", "//foo:gen"),
            ]
        );
    }

    #[test]
    fn merge_section_preserves_unattributed_lines() {
        let existing = format!("{BEGIN_MARKER}\n/legacy\n{END_MARKER}\n");
        let fresh = vec![attributed("/foo/new.go", "//foo:gen")];
        let matcher = Matcher::PackagePrefix(PkgBuf::from("foo"));

        let merged = merge_section(&existing, fresh, &matcher);
        assert_eq!(
            merged,
            vec![attributed("/foo/new.go", "//foo:gen"), bare("/legacy")]
        );
    }
}
