//! Generation of the heph-managed `.gitignore` section.
//!
//! Codegen targets with `codegen = copy` write generated files into the source
//! tree; those files must be gitignored. This module owns the *core* logic —
//! enumerating the patterns, rendering the marked section, diffing it against
//! the committed one and listing the recorded files a rewrite releases — so
//! `heph tool codegen gitignore`, its `--check` and `heph validate` agree. The
//! command layer only handles locks, file IO and reporting.

use std::sync::Arc;

use enclose::enclose;
use futures::TryStreamExt;

use crate::engine::Engine;
use crate::engine::discovery::Discovery;
use crate::engine::driver::targetdef::TargetDef;
use crate::engine::driver::targetdef::path::{CodegenMode, Content};
use hwalk::codegen_index::{Row, Section};
use crate::engine::query::skip_unresolvable;
use crate::engine::request_state::RequestState;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::{MatchResult, Matcher};

// The markers, the entry type and the one section parser live in `hwalk`, where
// the codegen index reads the same section; re-exported so engine callers keep
// their paths.
pub use hwalk::gitignore_section::{
    BEGIN_MARKER, BEGIN_MARKER_PREFIX, END_MARKER, GitignoreEntry, parse_section,
};

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
        // Always complete: the patterns are written to a file, and a short set
        // would delete the entries of every target the walk skipped — and make
        // `heph validate` report a correct file as out of date.
        let targets = self
            .codegen_copy_targets(rs, matcher, Discovery::Complete, false)
            .await?;
        Ok(targets.entries())
    }

    /// Resolve every target matching `matcher` that declares a `codegen =
    /// copy` output. Drains the addr stream first, then fans the def fetches
    /// out in parallel (each shares parse work through the per-request
    /// memoizer, like `codegen_copy_overlaps`).
    ///
    /// `keep_going`: a def that fails to resolve is collected in
    /// [`CopyTargets::failed`] instead of failing the call — what `codegen
    /// status` needs to report a broken BUILD file without failing.
    pub async fn codegen_copy_targets(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
        discovery: Discovery,
        keep_going: bool,
    ) -> anyhow::Result<CopyTargets> {
        let addrs: Vec<Addr> = {
            let stream = Arc::clone(&self).query(rs.clone(), matcher, discovery);
            tokio::pin!(stream);
            let mut v = Vec::new();
            while let Some(addr) = stream.try_next().await? {
                v.push(addr);
            }
            v
        };

        let fail_fast = rs.fail_fast() && !keep_going;
        let futs = addrs.iter().map(|addr| {
            enclose!((self => engine, rs, addr) async move {
                // A listed candidate that doesn't resolve standalone (e.g. go's
                // `//pkg:build` for a non-main package, resolved only as an
                // in-context dep) emits no codegen-copy output. The helper rather
                // than a `query_*` stream: the fan-out below reports *every*
                // failure, and a stream stops at the first.
                let def = match skip_unresolvable(&addr, engine.get_def(rs, &addr).await) {
                    Ok(Some(def)) => def,
                    Ok(None) => return Ok(None),
                    Err(e) if keep_going => return Ok(Some(Err((addr, e)))),
                    Err(e) => return Err(e),
                };
                let has_copy = def
                    .target_def
                    .outputs
                    .iter()
                    .flat_map(|output| output.paths.iter())
                    .any(|path| path.codegen_tree == CodegenMode::Copy);
                Ok::<_, anyhow::Error>(
                    has_copy.then(|| Ok((addr, Arc::clone(&def.target_def)))),
                )
            })
        });
        let per_target = crate::engine::fanout::join_all_failable(futs, fail_fast).await?;
        let mut out = CopyTargets::default();
        for r in per_target.into_iter().flatten() {
            match r {
                Ok(t) => out.targets.push(t),
                Err(f) => out.failed.push(f),
            }
        }
        out.targets.sort_by_cached_key(|(a, _)| a.format());
        out.failed.sort_by_cached_key(|(a, _)| a.format());
        Ok(out)
    }
}

/// The targets declaring `codegen = copy` outputs, as
/// [`Engine::codegen_copy_targets`] resolved them.
#[derive(Default)]
pub struct CopyTargets {
    /// Sorted by addr.
    pub targets: Vec<(Addr, Arc<TargetDef>)>,
    /// Defs that failed to resolve (only with `keep_going`), sorted by addr.
    pub failed: Vec<(Addr, anyhow::Error)>,
}

impl std::fmt::Debug for CopyTargets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopyTargets")
            .field(
                "targets",
                &self.targets.iter().map(|(a, _)| a.format()).collect::<Vec<_>>(),
            )
            .field("failed", &self.failed)
            .finish()
    }
}

impl CopyTargets {
    /// The section lines these targets emit, sorted and deduplicated.
    pub fn entries(&self) -> Vec<GitignoreEntry> {
        let entries = self
            .targets
            .iter()
            .flat_map(|(addr, def)| {
                def.outputs
                    .iter()
                    .flat_map(|output| output.paths.iter())
                    .filter(|path| path.codegen_tree == CodegenMode::Copy)
                    .map(|path| GitignoreEntry {
                        pattern: content_to_pattern(&path.content),
                        addr: Some(addr.clone()),
                    })
            })
            .collect();
        normalize(entries)
    }

    /// The def of `addr` (formatted), when it declares a copy output.
    pub fn get(&self, addr: &str) -> Option<&Arc<TargetDef>> {
        self.targets
            .binary_search_by(|(a, _)| a.format().as_str().cmp(addr))
            .ok()
            .and_then(|i| self.targets.get(i))
            .map(|(_, d)| d)
    }
}

/// Why a section is stale, as `--json` spells it (sorted by that spelling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StaleReason {
    /// The lines differ from what the defs emit.
    Lines,
    /// Markers are present but do not delimit a section.
    Malformed,
    /// The BEGIN line is an older heph's text (Decision 21).
    OldMarker,
}

impl StaleReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lines => "lines",
            Self::Malformed => "malformed",
            Self::OldMarker => "old_marker",
        }
    }
}

/// What rewriting a section with a set of entries would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionDiff {
    /// Lines the rewrite adds, sorted.
    pub added: Vec<GitignoreEntry>,
    /// Lines the rewrite removes, sorted.
    pub removed: Vec<GitignoreEntry>,
    /// Empty when the section is current; sorted otherwise.
    pub reasons: Vec<StaleReason>,
    /// The whole `.gitignore` after the rewrite.
    pub rendered: String,
}

impl SectionDiff {
    /// The rewrite changes the file: `--check` and `validate` fail.
    pub fn is_stale(&self) -> bool {
        !self.reasons.is_empty()
    }
}

/// Diff the `.gitignore` text `existing` against the section `entries` would
/// render. Stale is exactly "[`render`] changes the file"; the reasons say
/// why: an older marker, a malformed section, and otherwise (or as well)
/// lines that differ.
pub fn diff_section(existing: &str, entries: &[GitignoreEntry]) -> SectionDiff {
    let rendered = render(existing, entries);
    let parsed = hwalk::gitignore_section::parse_section_bytes(existing.as_bytes());
    let mut old: Vec<GitignoreEntry> = parsed.lines.iter().map(|l| l.entry.clone()).collect();
    old.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    let new = normalize(entries.to_vec());
    let (added, removed) = sorted_difference(&new, &old);

    let mut reasons = Vec::new();
    if rendered != existing {
        if parsed.old_marker {
            reasons.push(StaleReason::OldMarker);
        }
        let markers = existing.contains(BEGIN_MARKER_PREFIX) || existing.contains(END_MARKER);
        if !parsed.present && markers {
            reasons.push(StaleReason::Malformed);
        }
        if !added.is_empty() || !removed.is_empty() || reasons.is_empty() {
            reasons.push(StaleReason::Lines);
        }
        reasons.sort_by_key(|r| r.as_str());
    }
    SectionDiff {
        added,
        removed,
        reasons,
        rendered,
    }
}

/// `(a − b, b − a)` of two sorted entry lists, as multisets.
fn sorted_difference(
    a: &[GitignoreEntry],
    b: &[GitignoreEntry],
) -> (Vec<GitignoreEntry>, Vec<GitignoreEntry>) {
    use std::cmp::Ordering;

    let (mut only_a, mut only_b) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while let (Some(x), Some(y)) = (a.get(i), b.get(j)) {
        match x.sort_key().cmp(&y.sort_key()) {
            Ordering::Less => {
                only_a.push(x.clone());
                i += 1;
            }
            Ordering::Greater => {
                only_b.push(y.clone());
                j += 1;
            }
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    only_a.extend(a.iter().skip(i).cloned());
    only_b.extend(b.iter().skip(j).cloned());
    (only_a, only_b)
}

/// The rows a section rewrite releases: verified rows (not pending) the old
/// section covered and the new one does not. The caller keeps those whose
/// file is still there, and lists them.
pub fn released_rows<'r>(
    rows: impl IntoIterator<Item = &'r Row>,
    old: &Section,
    new: &Section,
) -> Vec<&'r Row> {
    let mut out: Vec<&Row> = rows
        .into_iter()
        .filter(|r| r.pending_hash.is_none())
        .filter(|r| {
            old.match_point(&r.path, false).is_territory()
                && !new.match_point(&r.path, false).is_territory()
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The rows `section` covers whose `seen_in_section` is not yet set, by addr:
/// what a section rewrite marks, each target's under its own lock.
pub fn newly_seen<'r>(
    rows: impl IntoIterator<Item = &'r Row>,
    section: &Section,
) -> std::collections::BTreeMap<String, Vec<Vec<u8>>> {
    let mut out: std::collections::BTreeMap<String, Vec<Vec<u8>>> = Default::default();
    for r in rows {
        if !r.seen_in_section && section.match_point(&r.path, false).is_territory() {
            out.entry(r.addr.clone()).or_default().push(r.path.clone());
        }
    }
    for paths in out.values_mut() {
        paths.sort();
    }
    out
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
/// A `DirPath` is emitted *without* a trailing slash. In gitignore syntax a
/// trailing `/` restricts the pattern to directories, and a `codegen = copy`
/// directory output is materialized as a **symlink** into the cache — which git
/// treats as a file, not a directory — so `/gen/` would fail to ignore it and the
/// link would show up untracked. `/gen` matches both spellings.
///
/// Reused by `validate` as the canonical, normalized output-path key for
/// detecting overlapping `codegen = copy` outputs across targets; its overlap
/// tests trim trailing slashes anyway, so they are unaffected.
pub(crate) fn content_to_pattern(content: &Content) -> String {
    match content {
        Content::FilePath(p) => format!("/{}", p.trim_start_matches('/')),
        Content::DirPath(p) => format!("/{}", p.trim_start_matches('/').trim_end_matches('/')),
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
    use crate::engine::Config;
    use hmodel::htaddr::parse_addr;
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
        // No trailing slash: a gitignore `dir/` pattern only matches real
        // directories, and a codegen dir output is materialized as a symlink.
        assert_eq!(
            content_to_pattern(&Content::DirPath("foo/gen".into())),
            "/foo/gen"
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
    fn render_emits_target_comment_above_pattern() {
        let out = render("", &[attributed("/foo/gen.go", "//foo:gen")]);
        assert_eq!(
            out,
            format!("{BEGIN_MARKER}\n# //foo:gen\n/foo/gen.go\n{END_MARKER}\n")
        );
    }

    /// Regression: attribution must be a full-line comment, never trailing.
    /// git only honors `#` as a comment at the start of a line, so a
    /// `pattern # //pkg:target` line is the literal pattern `pattern # //pkg:target`
    /// and fails to ignore `pattern`. Every non-comment line must be a bare,
    /// git-valid pattern with no inline `#`.
    #[test]
    fn render_never_emits_inline_comment() {
        let out = render(
            "",
            &[
                attributed("/foo/gen.go", "//foo:gen"),
                attributed("/bar/gen.go", "//bar:gen"),
            ],
        );
        for line in out.lines() {
            if line.starts_with('#') {
                continue;
            }
            assert!(
                !line.contains(" # "),
                "pattern line carries an inline comment git would treat as literal: {line:?}"
            );
        }
        // The pattern lines are the bare, anchored paths.
        assert!(out.contains("\n/foo/gen.go\n"));
        assert!(out.contains("\n/bar/gen.go\n"));
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

    fn section_of(entries: &[GitignoreEntry]) -> String {
        render("", entries)
    }

    #[test]
    fn diff_of_a_current_section_is_not_stale() {
        let entries = [attributed("/a/gen.go", "//a:gen")];
        let d = diff_section(&section_of(&entries), &entries);
        assert!(!d.is_stale());
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn diff_lists_added_and_removed_lines_with_their_targets() {
        let existing = section_of(&[
            attributed("/a/gen.go", "//a:gen"),
            attributed("/b/old.go", "//b:gone"),
        ]);
        let entries = [
            attributed("/a/gen.go", "//a:gen"),
            attributed("/c/new.go", "//c:gen"),
        ];
        let d = diff_section(&existing, &entries);
        assert_eq!(d.reasons, vec![StaleReason::Lines]);
        assert_eq!(d.added, vec![attributed("/c/new.go", "//c:gen")]);
        assert_eq!(d.removed, vec![attributed("/b/old.go", "//b:gone")]);
        assert_eq!(d.rendered, render(&existing, &entries));
    }

    #[test]
    fn diff_of_an_old_marker_with_current_lines_says_only_old_marker() {
        let entries = [attributed("/a/gen.go", "//a:gen")];
        let existing = section_of(&entries).replace(
            BEGIN_MARKER,
            "# BEGIN heph-generated (managed by `heph tool gen-gitignore` — do not edit)",
        );
        let d = diff_section(&existing, &entries);
        assert_eq!(d.reasons, vec![StaleReason::OldMarker]);
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn diff_of_an_old_marker_and_changed_lines_sorts_its_reasons() {
        let existing = section_of(&[attributed("/a/gen.go", "//a:gen")])
            .replace(BEGIN_MARKER, "# BEGIN heph-generated (old)");
        let d = diff_section(&existing, &[]);
        assert_eq!(d.reasons, vec![StaleReason::Lines, StaleReason::OldMarker]);
        assert_eq!(d.removed, vec![attributed("/a/gen.go", "//a:gen")]);
    }

    #[test]
    fn diff_of_markers_without_a_section_is_malformed() {
        // END missing: the parser reads no section, and the rewrite appends one.
        let existing = format!("{BEGIN_MARKER}\n/a\n");
        let d = diff_section(&existing, &[]);
        assert_eq!(d.reasons, vec![StaleReason::Malformed]);
        assert!(d.added.is_empty() && d.removed.is_empty());

        let existing = format!("{END_MARKER}\n{BEGIN_MARKER}\n");
        let d = diff_section(&existing, &[attributed("/a", "//a:gen")]);
        assert_eq!(d.reasons, vec![StaleReason::Lines, StaleReason::Malformed]);
    }

    #[test]
    fn diff_counts_a_duplicate_line_once_per_copy() {
        // Two targets emitting one path stay two lines; dropping one is a removal.
        let existing = section_of(&[attributed("/x", "//a:gen"), attributed("/x", "//b:gen")]);
        let d = diff_section(&existing, &[attributed("/x", "//a:gen")]);
        assert_eq!(d.removed, vec![attributed("/x", "//b:gen")]);
        assert!(d.added.is_empty());
    }

    fn row(path: &str, addr: &str) -> Row {
        Row::new(path.as_bytes(), addr, hwalk::codegen_index::RowKind::File)
    }

    #[test]
    fn released_rows_are_the_covered_rows_the_new_section_drops() {
        let old = Section::compile(
            section_of(&[attributed("/a/x.go", "//a:gen"), attributed("/b", "//b:gen")])
                .as_bytes(),
        );
        let new = Section::compile(section_of(&[attributed("/b", "//b:gen")]).as_bytes());
        let mut pending = row("a/y.go", "//a:gen");
        pending.pending_hash = Some("h".into());
        let rows = [
            row("b/k.go", "//b:gen"),
            row("a/x.go", "//a:gen"),
            row("c/never.go", "//c:gen"),
            pending,
        ];
        let released: Vec<&[u8]> = released_rows(&rows, &old, &new)
            .into_iter()
            .map(|r| r.path.as_slice())
            .collect();
        assert_eq!(released, vec![b"a/x.go".as_slice()]);
    }

    #[test]
    fn newly_seen_groups_unmarked_covered_rows_by_target() {
        let section = Section::compile(
            section_of(&[attributed("/a", "//a:gen"), attributed("/b", "//b:gen")]).as_bytes(),
        );
        let mut seen = row("b/s.go", "//b:gen");
        seen.seen_in_section = true;
        let rows = [
            row("a/2.go", "//a:gen"),
            row("a/1.go", "//a:gen"),
            seen,
            row("c/out.go", "//c:gen"),
        ];
        let got = newly_seen(&rows, &section);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got.get("//a:gen"),
            Some(&vec![b"a/1.go".to_vec(), b"a/2.go".to_vec()])
        );
    }

    /// A provider that *lists* an addr but returns `NotFound` from `get` —
    /// mimicking the go provider's `//pkg:build` for a non-main package, which
    /// resolves only as an in-context dep. The gitignore walk must skip such a
    /// target rather than surface its `get_def` failure. This is the bug behind
    /// `heph tool gen-gitignore //pkg/...` erroring with `target not found:
    /// //pkg:build@…` for a library-only go package.
    struct GhostProvider;

    impl crate::engine::provider::Provider for GhostProvider {
        fn config(
            &self,
            _req: crate::engine::provider::ConfigRequest,
        ) -> anyhow::Result<crate::engine::provider::ConfigResponse> {
            Ok(crate::engine::provider::ConfigResponse {
                name: "ghost".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: crate::engine::provider::ListRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<Item = anyhow::Result<crate::engine::provider::ListResponse>>
                        + Send,
                >,
            >,
        > {
            let pkg = req.package.clone();
            Box::pin(async move {
                let items: Vec<anyhow::Result<crate::engine::provider::ListResponse>> =
                    if pkg.as_str() == "virt" {
                        vec![Ok(crate::engine::provider::ListResponse::addr_only(
                            Addr::new(pkg, "build".to_string(), Default::default()),
                        ))]
                    } else {
                        vec![]
                    };
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: crate::engine::provider::ListPackagesRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<
                            Item = anyhow::Result<crate::engine::provider::ListPackageResponse>,
                        > + Send,
                >,
            >,
        > {
            Box::pin(async {
                let items: Vec<anyhow::Result<crate::engine::provider::ListPackageResponse>> =
                    vec![Ok(crate::engine::provider::ListPackageResponse {
                        pkg: PkgBuf::from("virt"),
                    })];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: crate::engine::provider::GetRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::engine::provider::GetResponse, crate::engine::provider::GetError>,
        > {
            // Listed but unresolvable standalone.
            Box::pin(async { Err(crate::engine::provider::GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: crate::engine::provider::ProbeRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::provider::ProbeResponse>>
        {
            Box::pin(async { Ok(crate::engine::provider::ProbeResponse { states: vec![] }) })
        }
    }

    /// The patterns are written to a file, so the walk behind them is always
    /// complete: one candidate that cannot be resolved fails the call instead
    /// of producing a file missing that candidate's entries.
    #[tokio::test]
    async fn gitignore_patterns_refuse_a_partial_selection() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine.register_provider(|_| {
            Box::new(
                crate::engine::fault_provider::FaultProvider::unresolvable(vec![
                    parse_addr("//gen:bad").expect("addr"),
                ])
                .expect("fault provider"),
            )
        })?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();
        let res = Arc::clone(&engine)
            .codegen_copy_gitignore_patterns(rs, &Matcher::TreeOutputTo(PkgBuf::from("")))
            .await;
        let err = res.expect_err("a partial set must not reach the file");
        assert!(format!("{err:#}").contains("go list"), "{err:#}");
        Ok(())
    }

    // Several targets are enumerated concurrently, so completion order is
    // nondeterministic. The result must not be: every entry is present exactly
    // once, sorted by pattern, and attributed to its emitting target.
    #[tokio::test]
    async fn entries_from_many_targets_are_complete_and_sorted() -> anyhow::Result<()> {
        use hmodel::htaddr::Addr;
        use std::collections::HashMap;

        fn codegen_target(addr: &str, out: &str) -> hbuiltins::pluginstatictarget::Target {
            let mut outs = HashMap::new();
            outs.insert(String::new(), vec![out.to_string()]);
            hbuiltins::pluginstatictarget::Target {
                addr: addr.to_string(),
                driver: "exec".to_string(),
                run: Some("true".to_string()),
                out: outs,
                codegen: Some("copy".to_string()),
                ..Default::default()
            }
        }

        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine.register_managed_driver(|_| {
            Box::new(hplugin_exec::pluginexec::Driver::new_exec().with_host_path())
        })?;
        // Declared in an order that does not match the sorted pattern order, so a
        // stable result cannot come from insertion order alone.
        let provider = hbuiltins::pluginstatictarget::Provider::new(vec![
            codegen_target("//z:t", "z_gen.go"),
            codegen_target("//a:t", "a_gen.go"),
            codegen_target("//m:t", "m_gen.go"),
        ])?;
        engine.register_provider(move |_| Box::new(provider))?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let entries = Arc::clone(&engine)
            .codegen_copy_gitignore_patterns(rs, &Matcher::TreeOutputTo(PkgBuf::from("")))
            .await?;

        let got: Vec<(&str, String)> = entries
            .iter()
            .map(|e| {
                (
                    e.pattern.as_str(),
                    e.addr.as_ref().map(Addr::format).unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("/a/a_gen.go", "//a:t".to_string()),
                ("/m/m_gen.go", "//m:t".to_string()),
                ("/z/z_gen.go", "//z:t".to_string()),
            ],
            "entries must be complete, attributed, and pattern-sorted: {entries:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn listed_but_unresolvable_target_is_skipped() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine.register_provider(|_| Box::new(GhostProvider))?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        // The listed `//virt:build` resolves straight to the walk (no spec
        // resolution), so its `get_def` NotFound is what the skip must absorb:
        // no error, no entries.
        let entries = Arc::clone(&engine)
            .codegen_copy_gitignore_patterns(rs, &Matcher::PackagePrefix(PkgBuf::from("virt")))
            .await?;

        assert!(entries.is_empty(), "ghost target skipped: {entries:?}");
        Ok(())
    }
}
