//! Generation of the heph-managed `.gitignore` section.
//!
//! Codegen targets with `codegen = copy` write generated files into the source
//! tree; those files must be gitignored. This module owns the *core* logic —
//! enumerating the patterns and rendering the marked section — so it can be
//! reused by the `gen-gitignore` command and by future validations (e.g. a
//! CI check that the committed `.gitignore` is up to date). The command layer
//! only handles file IO and progress reporting.

use std::sync::Arc;

use futures::TryStreamExt;

use crate::engine::Engine;
use crate::engine::driver::targetdef::path::{CodegenMode, Content};
use crate::engine::request_state::RequestState;
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;

/// Markers delimiting the heph-managed region. Lines between them (inclusive)
/// are owned by heph and rewritten on every run; everything outside is
/// preserved verbatim.
pub const BEGIN_MARKER: &str =
    "# BEGIN heph-generated (managed by `heph gen-gitignore` — do not edit)";
pub const END_MARKER: &str = "# END heph-generated";

impl Engine {
    /// Enumerate the root-anchored `.gitignore` patterns for every
    /// `codegen = copy` output in the workspace. Sorted and deduplicated so the
    /// result is deterministic and directly diffable against an existing file.
    pub async fn codegen_copy_gitignore_patterns(
        self: Arc<Self>,
        rs: Arc<RequestState>,
    ) -> anyhow::Result<Vec<String>> {
        // Empty-package `TreeOutputTo` selects every target whose codegen tree
        // lands anywhere in the workspace (any codegen mode). The per-path
        // `CodegenMode::Copy` filter keeps only the copied outputs — those are
        // the ones written into the tree and thus need gitignoring.
        let matcher = Matcher::TreeOutputTo(PkgBuf::from(""));

        let mut entries: Vec<String> = Vec::new();
        let stream = Arc::clone(&self).query(rs.clone(), &matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            let def = Arc::clone(&self).get_def(rs.clone(), &addr).await?;
            for output in &def.target_def.outputs {
                for path in &output.paths {
                    if path.codegen_tree == CodegenMode::Copy {
                        entries.push(content_to_pattern(&path.content));
                    }
                }
            }
        }
        entries.sort();
        entries.dedup();
        Ok(entries)
    }
}

/// Convert an output path into a root-anchored `.gitignore` pattern. Output
/// paths are already workspace-root-relative (package-rooted), so a leading
/// `/` anchors them precisely.
fn content_to_pattern(content: &Content) -> String {
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
pub fn render(existing: &str, entries: &[String]) -> String {
    let mut section = String::new();
    section.push_str(BEGIN_MARKER);
    section.push('\n');
    for e in entries {
        section.push_str(e);
        section.push('\n');
    }
    section.push_str(END_MARKER);

    match (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
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
        let out = render("", &["/a".into(), "/b".into()]);
        assert_eq!(out, format!("{BEGIN_MARKER}\n/a\n/b\n{END_MARKER}\n"));
    }

    #[test]
    fn render_appends_after_existing_content_with_blank_line() {
        let out = render("node_modules\n", &["/gen/".into()]);
        assert_eq!(
            out,
            format!("node_modules\n\n{BEGIN_MARKER}\n/gen/\n{END_MARKER}\n")
        );
    }

    #[test]
    fn render_inserts_newline_when_existing_lacks_trailing() {
        let out = render("node_modules", &["/gen/".into()]);
        assert_eq!(
            out,
            format!("node_modules\n\n{BEGIN_MARKER}\n/gen/\n{END_MARKER}\n")
        );
    }

    #[test]
    fn render_replaces_existing_section_preserving_surroundings() {
        let existing = format!("top\n{BEGIN_MARKER}\n/old\n{END_MARKER}\nbottom\n");
        let out = render(&existing, &["/new".into()]);
        assert_eq!(
            out,
            format!("top\n{BEGIN_MARKER}\n/new\n{END_MARKER}\nbottom\n")
        );
    }

    #[test]
    fn render_shrinks_section_when_entries_removed() {
        let existing = format!("{BEGIN_MARKER}\n/a\n/b\n/c\n{END_MARKER}\n");
        let out = render(&existing, &["/a".into()]);
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
        let first = render("keep\n", &["/a".into(), "/b".into()]);
        let second = render(&first, &["/a".into(), "/b".into()]);
        assert_eq!(first, second);
    }
}
