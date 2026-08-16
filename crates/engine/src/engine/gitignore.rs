//! Generation of the heph-managed `.gitignore` section.
//!
//! Codegen targets with `codegen = copy` write generated files into the source
//! tree; those files must be gitignored. This module owns the *core* logic —
//! enumerating the patterns and rendering the marked section — so it can be
//! reused by the `tool gen-gitignore` command and by future validations (e.g. a
//! CI check that the committed `.gitignore` is up to date). The command layer
//! only handles file IO and progress reporting.

use std::sync::Arc;

use enclose::enclose;
use futures::TryStreamExt;

use crate::engine::Engine;
use crate::engine::driver::targetdef::path::{CodegenMode, Content};
use crate::engine::query::skip_unresolvable;
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

/// Prefix marking an attribution comment line: `# //pkg:target`, rendered on its
/// own line *above* the pattern it annotates.
///
/// git only treats a `#` as a comment when it is the **first** character of the
/// line — a trailing `pattern # //pkg:target` is not a comment, it is the literal
/// pattern `pattern # //pkg:target` (space and hash included), which matches a
/// file of that exact name rather than `pattern`. So attribution must live on a
/// preceding full-line comment; it round-trips through [`parse_section`].
const COMMENT_PREFIX: &str = "# ";

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
    fn to_line(&self) -> String {
        match &self.addr {
            Some(addr) => format!("{COMMENT_PREFIX}{}\n{}", addr.format(), self.pattern),
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
        // Drain the addr stream first, then fan the def fetches out in parallel
        // (each shares parse work through the per-request memoizer, like
        // `codegen_copy_overlaps`). The sequential await-per-addr loop this
        // replaced serialized every `get_def`.
        let addrs: Vec<Addr> = {
            let stream = Arc::clone(&self).query(rs.clone(), matcher);
            tokio::pin!(stream);
            let mut v = Vec::new();
            while let Some(addr) = stream.try_next().await? {
                v.push(addr);
            }
            v
        };

        let fail_fast = rs.fail_fast();
        let futs = addrs.iter().map(|addr| {
            enclose!((self => engine, rs, addr) async move {
                // A listed candidate that doesn't resolve standalone (e.g. go's
                // `//pkg:build` for a non-main package, resolved only as an
                // in-context dep) emits no codegen-copy output. The helper rather
                // than a `query_*` stream: the fan-out below reports *every*
                // failure, and a stream stops at the first.
                let Some(def) = skip_unresolvable(&addr, engine.get_def(rs, &addr).await)? else {
                    return Ok(Vec::new());
                };
                let entries: Vec<GitignoreEntry> = def
                    .target_def
                    .outputs
                    .iter()
                    .flat_map(|output| output.paths.iter())
                    .filter(|path| path.codegen_tree == CodegenMode::Copy)
                    .map(|path| GitignoreEntry {
                        pattern: content_to_pattern(&path.content),
                        addr: Some(addr.clone()),
                    })
                    .collect();
                Ok::<Vec<GitignoreEntry>, anyhow::Error>(entries)
            })
        });
        let per_target = crate::engine::fanout::join_all_failable(futs, fail_fast).await?;
        let entries: Vec<GitignoreEntry> = per_target.into_iter().flatten().collect();
        Ok(normalize(entries))
    }
}

/// Extract the entries currently inside the heph-managed marker section of
/// `existing`. Returns an empty vec when no section is present.
///
/// The section is a sequence of pattern lines, each optionally preceded by a
/// `# //pkg:target` attribution comment on its own line. Walking is stateful: a
/// parseable attribution comment is held and attached to the next pattern line;
/// any other line (a bare pattern, or a comment that is not a valid target addr)
/// is preserved verbatim as an un-attributed entry. Marker and blank lines are
/// skipped.
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

    let mut entries = Vec::new();
    let mut pending: Option<Addr> = None;
    for line in existing[start..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(BEGIN_MARKER_PREFIX))
    {
        if let Some(rest) = line.strip_prefix('#') {
            // A `#` line is an attribution comment if the remainder parses as a
            // target addr; flush any dangling pending comment as a bare line
            // first so nothing is silently dropped, then hold this one.
            if let Ok(addr) = parse_addr(rest.trim()) {
                if let Some(prev) = pending.take() {
                    entries.push(GitignoreEntry {
                        pattern: format!("{COMMENT_PREFIX}{}", prev.format()),
                        addr: None,
                    });
                }
                pending = Some(addr);
            } else {
                entries.push(GitignoreEntry {
                    pattern: line.to_string(),
                    addr: None,
                });
            }
            continue;
        }
        entries.push(GitignoreEntry {
            pattern: line.to_string(),
            addr: pending.take(),
        });
    }
    // A trailing attribution comment with no following pattern is preserved.
    if let Some(prev) = pending {
        entries.push(GitignoreEntry {
            pattern: format!("{COMMENT_PREFIX}{}", prev.format()),
            addr: None,
        });
    }
    entries
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
                        vec![Ok(crate::engine::provider::ListResponse {
                            addr: Addr::new(pkg, "build".to_string(), Default::default()),
                        })]
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
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
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
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
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
