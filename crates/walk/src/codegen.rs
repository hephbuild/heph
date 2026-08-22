//! Which tree paths are *generated* rather than *source*.
//!
//! A `codegen = "copy"` target writes a net-new file into the workspace tree.
//! Nothing may source that file as raw input — not a `glob()`, not a `file()`,
//! not a provider's own directory scan — because its content already enters the
//! graph through its generator. Sourcing it again double-sources the bytes and,
//! when the generator's own inputs glob the same directory, feeds a target's
//! output back into its input.
//!
//! # Why a declaration and not a mark on the file
//!
//! This was previously a `user.heph.codegen` extended attribute stamped on each
//! written-back file. xattrs are **inode-scoped**, and the dominant way tools
//! rewrite a file is write-temp-then-`rename(2)` — a new inode. So `gofmt -w`,
//! `prettier`, `sed -i`, every editor save, `git checkout`, `cp` without `-p`,
//! `tar`/`rsync` without their xattr flags, and any filesystem without xattr
//! support all silently erased the mark. A file that lost it looked like source
//! on the very next walk, and the walk that observed the loss had no dependency
//! edge forcing the generator to run first — so nothing re-stamped it in time.
//!
//! The replacement is a *declaration* rather than a *mark*: the set of claimed
//! paths is read from the heph-managed section of the workspace-root
//! `.gitignore` — the same section `heph tool gen-gitignore` writes from the
//! declared `codegen = "copy"` output paths and `heph validate` checks for
//! freshness. That section is committed, survives every tool that touches the
//! tree (nothing about it is tied to an inode), is correct for a file that has
//! never been generated yet, and is reviewable in a diff.
//!
//! Reading it lives here, next to [`Ignore`](crate::Ignore), because the same
//! set has to reach every tree-walking plugin and this crate is the one they all
//! already depend on. Rendering that section stays in the engine, which is the
//! only place that can resolve target defs; the marker constants are defined
//! here so reader and writer cannot drift.

use anyhow::Context as _;
use std::path::Path;
use std::sync::Arc;
use wax::{Any, Glob, Program as _};

/// Stable prefix of the managed section's opening marker. Matching on the prefix
/// rather than the full line means changing the marker's parenthetical never
/// orphans an already-committed section.
pub const BEGIN_MARKER_PREFIX: &str = "# BEGIN heph-generated";

/// The managed section's closing marker.
pub const END_MARKER: &str = "# END heph-generated";

/// One claimed path pattern and the target that emits it.
#[derive(Debug)]
struct Claim {
    /// Compiled matcher for workspace-relative paths.
    glob: Glob<'static>,
    /// The emitting target's addr, from the section's attribution comment.
    /// `None` for an un-attributed (hand-written or legacy) line.
    owner: Option<String>,
}

/// The set of workspace-relative paths claimed as `codegen = "copy"` output.
///
/// Cheap to share (`Arc`) and immutable once built: it is loaded once per
/// process, at engine construction, and handed to every plugin that walks the
/// tree. A claim added to `.gitignore` mid-run therefore takes effect on the
/// next run — the same timing the xattr stamp had, since that too was written
/// after the walk that would have consulted it.
#[derive(Debug)]
pub struct CodegenClaims {
    claims: Vec<Claim>,
    /// Union of every claim's glob — the fast path, one match for the common
    /// "is this path generated?" question asked once per walked file.
    any: Arc<Any<'static>>,
}

impl Default for CodegenClaims {
    fn default() -> Self {
        Self::empty()
    }
}

impl CodegenClaims {
    /// Claims nothing. The right value for a workspace with no codegen targets,
    /// and for the non-engine call sites (LSP, unit tests) that have no root to
    /// read from.
    pub fn empty() -> Self {
        Self {
            claims: Vec::new(),
            any: Arc::new(wax::any(Vec::<Glob<'static>>::new()).expect("empty any is valid")),
        }
    }

    /// Read the claim set from the heph-managed section of `<root>/.gitignore`.
    ///
    /// Never fails: a missing file, an unreadable one, or a malformed pattern
    /// yields an empty (or partial) set with a warning, because a workspace that
    /// cannot be read is not a workspace where refusing to build is the helpful
    /// answer. The cost of an empty set is that generated files are sourced as
    /// raw input — which `heph validate` reports as a stale `.gitignore`, and
    /// which the write-back warns about the moment it writes an unclaimed path.
    pub fn load(root: &Path) -> Self {
        let path = root.join(".gitignore");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::empty(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "cannot read .gitignore; codegen outputs will not be excluded from source globs"
                );
                return Self::empty();
            }
        };
        match Self::from_gitignore(&text) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "cannot parse the heph-managed .gitignore section; \
                     codegen outputs will not be excluded from source globs"
                );
                Self::empty()
            }
        }
    }

    /// Parse the heph-managed section out of a `.gitignore`'s full text.
    ///
    /// The section is a sequence of root-anchored pattern lines, each optionally
    /// preceded by a `# //pkg:target` attribution comment on its own line — the
    /// shape the engine's renderer emits. Lines outside the markers are ignored:
    /// a user's own ignore rules say nothing about what is generated.
    #[expect(
        clippy::string_slice,
        reason = "slice indices come from `find` on ASCII markers — always char-aligned"
    )]
    pub fn from_gitignore(text: &str) -> anyhow::Result<Self> {
        let (Some(start), Some(end)) = (text.find(BEGIN_MARKER_PREFIX), text.find(END_MARKER))
        else {
            return Ok(Self::empty());
        };
        if end < start {
            return Ok(Self::empty());
        }
        Self::from_lines(text[start..end].lines())
    }

    /// Build from the section's lines (markers may be present; they are skipped).
    fn from_lines<'a>(lines: impl Iterator<Item = &'a str>) -> anyhow::Result<Self> {
        let mut claims = Vec::new();
        let mut pending: Option<String> = None;
        for line in lines
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with(BEGIN_MARKER_PREFIX))
        {
            if let Some(rest) = line.strip_prefix('#') {
                // Attribution comments name the emitting target; anything else in
                // the section is a comment we don't own. Held for the next
                // pattern line, matching how the renderer lays them out.
                let rest = rest.trim();
                pending = rest.starts_with("//").then(|| rest.to_string());
                continue;
            }
            // A negation un-ignores a path — the renderer never emits one, and
            // treating it as a claim would invert its meaning.
            if line.starts_with('!') {
                pending = None;
                continue;
            }
            let owner = pending.take();
            for pattern in expand_pattern(line) {
                let glob = Glob::new(&pattern)
                    .map(Glob::into_owned)
                    .with_context(|| format!("invalid codegen claim pattern '{pattern}'"))?;
                claims.push(Claim {
                    glob,
                    owner: owner.clone(),
                });
            }
        }
        let any = Arc::new(
            wax::any(claims.iter().map(|c| c.glob.clone()).collect::<Vec<_>>())
                .context("compiling codegen claim patterns")?,
        );
        Ok(Self { claims, any })
    }

    /// True when nothing is claimed — the whole check can be skipped.
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }

    /// True if the workspace-relative path `rel` is generated by a
    /// `codegen = "copy"` target, and so must never be sourced as raw input.
    pub fn claims(&self, rel: &Path) -> bool {
        !self.claims.is_empty() && self.any.is_match(rel)
    }

    /// The addr of the target that emits `rel`, when the claim carries an
    /// attribution comment. Diagnostics only — [`Self::claims`] is the decision.
    pub fn owner(&self, rel: &Path) -> Option<&str> {
        self.claims
            .iter()
            .find(|c| c.glob.is_match(rel))
            .and_then(|c| c.owner.as_deref())
    }
}

/// Translate one root-anchored `.gitignore` pattern into the wax globs that
/// match the same workspace-relative paths.
///
/// Two differences from gitignore syntax matter here:
///  - a leading `/` anchors to the repo root, which is already what a
///    workspace-relative match means, so it is stripped;
///  - a directory pattern ignores the whole subtree implicitly, while wax
///    matches only the named path — so every pattern also gets a `…/**` form.
///    A file pattern's `…/**` form matches nothing, which costs one extra glob
///    and never a wrong answer.
fn expand_pattern(pattern: &str) -> impl Iterator<Item = String> {
    let base = pattern
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    let subtree = format!("{base}/**");
    [base, subtree].into_iter().filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    fn section(body: &str) -> CodegenClaims {
        CodegenClaims::from_gitignore(&format!(
            "target/\n\n{BEGIN_MARKER_PREFIX} (managed)\n{body}\n{END_MARKER}\nnode_modules/\n"
        ))
        .expect("valid section")
    }

    #[test]
    fn file_claim_matches_only_that_path() {
        let c = section("# //fmt:generated\n/fmt/generated.txt");
        assert!(c.claims(p("fmt/generated.txt")));
        assert!(!c.claims(p("fmt/other.txt")));
        assert!(!c.claims(p("generated.txt")));
    }

    #[test]
    fn dir_claim_matches_the_dir_and_its_subtree() {
        let c = section("# //pkg:gen\n/pkg/gen");
        assert!(c.claims(p("pkg/gen")), "the directory itself");
        assert!(c.claims(p("pkg/gen/a.go")), "a file directly inside");
        assert!(c.claims(p("pkg/gen/deep/b.go")), "a file nested inside");
        assert!(!c.claims(p("pkg/generated.go")), "a sibling with a prefix");
    }

    #[test]
    fn glob_claim_matches_by_pattern() {
        let c = section("# //pkg:proto\n/pkg/**/*.pb.go");
        assert!(c.claims(p("pkg/a.pb.go")));
        assert!(c.claims(p("pkg/sub/b.pb.go")));
        assert!(!c.claims(p("pkg/a.go")));
    }

    #[test]
    fn owner_is_the_attributed_target() {
        let c = section("# //fmt:generated\n/fmt/generated.txt\n# //pkg:gen\n/pkg/gen");
        assert_eq!(c.owner(p("fmt/generated.txt")), Some("//fmt:generated"));
        assert_eq!(c.owner(p("pkg/gen/a.go")), Some("//pkg:gen"));
        assert_eq!(c.owner(p("src/main.rs")), None);
    }

    #[test]
    fn unattributed_line_still_claims() {
        // A legacy or hand-written line inside the managed section is honored as
        // a claim; only its provenance is unknown.
        let c = section("/legacy/out.txt");
        assert!(c.claims(p("legacy/out.txt")));
        assert_eq!(c.owner(p("legacy/out.txt")), None);
    }

    #[test]
    fn patterns_outside_the_markers_are_not_claims() {
        // `target/` and `node_modules/` sit outside the managed section in
        // `section()`'s scaffolding — a user's own ignores say nothing about what
        // is generated.
        let c = section("/pkg/gen");
        assert!(!c.claims(p("target/debug/heph")));
        assert!(!c.claims(p("node_modules/x/index.js")));
    }

    #[test]
    fn no_section_claims_nothing() {
        let c = CodegenClaims::from_gitignore("target/\nnode_modules/\n").expect("valid");
        assert!(c.is_empty());
        assert!(!c.claims(p("target/debug/heph")));
    }

    #[test]
    fn negation_is_not_a_claim() {
        let c = section("# //pkg:gen\n!/pkg/keep.txt\n/pkg/gen");
        assert!(!c.claims(p("pkg/keep.txt")));
        assert!(c.claims(p("pkg/gen")));
    }

    #[test]
    fn empty_claims_never_match() {
        let c = CodegenClaims::empty();
        assert!(c.is_empty());
        assert!(!c.claims(p("anything")));
        assert!(!c.claims(p("")));
    }

    #[test]
    fn malformed_pattern_is_an_error_not_a_silent_drop() {
        let err = CodegenClaims::from_gitignore(&format!(
            "{BEGIN_MARKER_PREFIX}\n/pkg/<bad\n{END_MARKER}\n"
        ));
        assert!(err.is_err(), "an unparseable claim must not be dropped");
    }
}
