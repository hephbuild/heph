//! Path relationships, for deciding whether two declared paths collide, and
//! [`lexical`] normalization.
//!
//! Pure string arithmetic over already-normalized, same-frame paths — it touches
//! no filesystem and resolves no symlinks. Callers are responsible for putting
//! both paths in the same frame first (both workspace-relative, or both
//! absolute); comparing paths from different frames is a caller bug this cannot
//! detect.

use std::path::{Component, Path, PathBuf};

/// `p` with `.` and `..` resolved as text, as git does: no filesystem access, so
/// a path that does not exist yet works, and a symlink before a `..` is not
/// followed.
///
/// A `..` pops the previous component. At the root it is dropped, so `/..` is
/// `/`. A relative path keeps a `..` it has nothing to pop, so `../a` stays
/// `../a` and `a/../../b` is `../b`: dropping it would name another path.
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                // Empty, or already a leading `..`: nothing to pop.
                Some(Component::ParentDir | Component::CurDir) | None => {
                    out.push(Component::ParentDir);
                }
            },
            other => out.push(other),
        }
    }
    out
}

/// Components of a path, with the noise removed: empty segments (a leading `/`,
/// a doubled `//`, a trailing `/`) and `.` segments, which name the same
/// directory as their parent.
///
/// `..` is *kept* as a literal component rather than resolved, because resolving
/// it needs the filesystem — `a/../b` and `b` are only the same path when `a` is
/// not a symlink, and this module touches no disk.
fn comps(p: &str) -> Vec<&str> {
    p.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

/// True when `ancestor` is a strict parent directory of `descendant`.
///
/// Compared per component, so `.cache/go` is not an ancestor of
/// `.cache/golang` — a bare `starts_with` says it is, and the resulting
/// collision error names two paths a reader can see are unrelated.
pub fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    let (a, d) = (comps(ancestor), comps(descendant));
    !a.is_empty() && d.len() > a.len() && d.iter().zip(a.iter()).all(|(x, y)| x == y)
}

/// True when two paths collide in the tree: the same path however it is spelled,
/// or one is an ancestor directory of the other.
///
/// **An empty path collides with nothing, including another empty path.** Empty
/// means *no path* — an unmounted scratch cache, say — not the root. A naive
/// component-prefix comparison returns "overlaps" here, because zipping against
/// an empty list is vacuously true, which turns the absence of a path into a
/// collision with every path there is.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let (x, y) = (comps(a), comps(b));
    if x.is_empty() || y.is_empty() {
        return false;
    }
    // Zip stops at the shorter, which is exactly the prefix comparison wanted —
    // and both are non-empty by the guard above, so it cannot be vacuous.
    x.iter().zip(y.iter()).all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_overlaps_itself_however_it_is_spelled() {
        assert!(paths_overlap("gen", "gen"));
        // A file and a same-named directory still clash.
        assert!(paths_overlap("gen/", "gen"));
        // And `.` names the same directory as its parent.
        assert!(paths_overlap("gen", "./gen"));
        assert!(paths_overlap("a//b", "a/b/"));
    }

    /// `..` stays literal. Resolving it needs the filesystem — `a/../b` and `b`
    /// are the same path only if `a` is not a symlink — and this module touches
    /// no disk, so it must not pretend to know.
    #[test]
    fn parent_segments_are_not_resolved() {
        assert!(!paths_overlap("a/../b", "b"));
    }

    #[test]
    fn containment_is_an_overlap_in_both_directions() {
        assert!(paths_overlap("gen", "gen/a.go"));
        assert!(paths_overlap("gen/a.go", "gen"));
    }

    /// The case a bare `starts_with` gets wrong. An error naming these two as
    /// colliding is nonsense the author cannot act on.
    #[test]
    fn a_shared_prefix_is_not_containment() {
        assert!(!paths_overlap(".cache/go", ".cache/golang"));
        assert!(!is_ancestor("/a", "/ab"));
        assert!(!paths_overlap("gen", "generated"));
    }

    #[test]
    fn siblings_do_not_overlap() {
        assert!(!paths_overlap("a/x", "a/y"));
        assert!(!paths_overlap("a", "b"));
    }

    /// Empty means *no path*, not the root. A component-wise prefix comparison
    /// returns "overlaps" here — zipping against an empty component list is
    /// vacuously true — which would make an unmounted cache collide with
    /// everything.
    #[test]
    fn an_absent_path_collides_with_nothing() {
        assert!(!paths_overlap("", "anything"));
        assert!(!paths_overlap("anything", ""));
        assert!(!paths_overlap("", ""));
    }

    #[test]
    fn lexical_resolves_dot_and_dotdot_as_text() {
        let l = |s: &str| lexical(Path::new(s));
        assert_eq!(l("/w/a/./b"), Path::new("/w/a/b"));
        assert_eq!(l("/w/a/../b"), Path::new("/w/b"));
        assert_eq!(l("/w/a/b/../../c"), Path::new("/w/c"));
        assert_eq!(l("a//b/"), Path::new("a/b"));
        assert_eq!(l("a/b/.."), Path::new("a"));
    }

    /// `..` at the root is the root: `/..` is `/`, and a path cannot climb
    /// above it however many `..` it has.
    #[test]
    fn lexical_cannot_climb_above_the_root() {
        let l = |s: &str| lexical(Path::new(s));
        assert_eq!(l("/.."), Path::new("/"));
        assert_eq!(l("/w/../../../x"), Path::new("/x"));
        assert_eq!(l("."), Path::new(""));
    }

    /// A relative path has no root to stop at: a `..` with nothing to pop is
    /// kept, or the result would name a different path.
    #[test]
    fn lexical_keeps_a_relative_paths_leading_dotdot() {
        let l = |s: &str| lexical(Path::new(s));
        assert_eq!(l("../a"), Path::new("../a"));
        assert_eq!(l(".."), Path::new(".."));
        assert_eq!(l("../../a"), Path::new("../../a"));
        assert_eq!(l("a/../../b"), Path::new("../b"));
        assert_eq!(l("./../a/./b/.."), Path::new("../a"));
        assert_eq!(l("../a/.."), Path::new(".."));
    }
}
