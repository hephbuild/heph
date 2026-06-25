use std::fmt;
use std::path::Path;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PkgBuf(String);

impl PkgBuf {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    pub fn has_prefix(&self, prefix: &PkgBuf) -> bool {
        let p = prefix.as_str();
        if p.is_empty() {
            return true;
        }
        self.0 == p || self.0.starts_with(&format!("{}/", p))
    }

    /// Iterator yielding this package, then each ancestor up to (and including)
    /// the root package `""`. For `a/b/c` yields `a/b/c`, `a/b`, `a`, `""`.
    /// For `""` yields just `""`.
    pub fn parent_packages(&self) -> impl Iterator<Item = PkgBuf> + '_ {
        let mut next: Option<&str> = Some(self.0.as_str());
        std::iter::from_fn(move || {
            let cur = next?;
            let out = PkgBuf::from(cur);
            next = if cur.is_empty() {
                None
            } else {
                Some(match cur.rsplit_once('/') {
                    Some((parent, _)) => parent,
                    None => "",
                })
            };
            Some(out)
        })
    }
}

/// Join a package-relative `rel` onto package `pkg`, returning a single
/// workspace-root-relative path. The result is lexically normalized: empty and
/// `.` segments are dropped and `..` segments are resolved against earlier
/// components, so callers never emit smells like `pkg/./sub` or `pkg/a/../b`.
/// Glob metacharacters in segments are preserved verbatim, and a single
/// trailing slash is kept when `rel` had one (so directory paths stay
/// distinguishable from file paths). Returns an error when a `..` segment would
/// escape the workspace root — declaring a path outside the workspace is always
/// a configuration mistake.
pub fn join_rel_checked(pkg: &str, rel: &str) -> anyhow::Result<String> {
    let joined = join_raw(pkg, rel);
    let mut out: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    anyhow::bail!("path '{joined}' escapes workspace root");
                }
            }
            other => out.push(other),
        }
    }
    Ok(finish(&out, &joined))
}

/// Concatenate `pkg` and `rel` with a single separator, skipping the separator
/// when either side is empty (root package, or a bare package reference).
fn join_raw(pkg: &str, rel: &str) -> String {
    if pkg.is_empty() {
        rel.to_string()
    } else if rel.is_empty() {
        pkg.to_string()
    } else {
        format!("{pkg}/{rel}")
    }
}

/// Re-join normalized segments, restoring a single trailing slash when the
/// pre-normalization `original` had one and the result is non-empty.
fn finish(out: &[&str], original: &str) -> String {
    let mut s = out.join("/");
    if original.ends_with('/') && !s.is_empty() {
        s.push('/');
    }
    s
}

impl fmt::Display for PkgBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PkgBuf {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for PkgBuf {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl From<String> for PkgBuf {
    fn from(s: String) -> PkgBuf {
        PkgBuf(s)
    }
}

impl From<&str> for PkgBuf {
    fn from(s: &str) -> PkgBuf {
        PkgBuf(s.to_string())
    }
}

impl PartialEq<str> for PkgBuf {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for PkgBuf {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for PkgBuf {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(s: &str) -> PkgBuf {
        PkgBuf::from(s)
    }

    #[test]
    fn exact_match() {
        assert!(pkg("foo/bar").has_prefix(&pkg("foo/bar")));
    }

    #[test]
    fn child_match() {
        assert!(pkg("foo/bar/baz").has_prefix(&pkg("foo/bar")));
        assert!(pkg("foo/bar/baz").has_prefix(&pkg("foo")));
    }

    #[test]
    fn no_partial_component_match() {
        assert!(!pkg("foo/bar/baz").has_prefix(&pkg("foo/ba")));
    }

    #[test]
    fn empty_prefix_matches_all() {
        assert!(pkg("").has_prefix(&pkg("")));
        assert!(pkg("foo/bar").has_prefix(&pkg("")));
        assert!(pkg("foo/bar/baz").has_prefix(&pkg("")));
    }

    #[test]
    fn parent_packages_walks_to_root() {
        let got: Vec<String> = pkg("a/b/c")
            .parent_packages()
            .map(|p| p.as_str().to_string())
            .collect();
        assert_eq!(got, vec!["a/b/c", "a/b", "a", ""]);
    }

    #[test]
    fn parent_packages_root_only() {
        let got: Vec<String> = pkg("")
            .parent_packages()
            .map(|p| p.as_str().to_string())
            .collect();
        assert_eq!(got, vec![""]);
    }

    fn join(pkg: &str, rel: &str) -> String {
        join_rel_checked(pkg, rel).expect("join should not escape root")
    }

    #[test]
    fn join_rel_collapses_dot_and_empty_segments() {
        // The bug that motivated this: a `./`-prefixed output declared in a BUILD
        // file used to land in the path verbatim as `pkg/./sub/file`.
        assert_eq!(
            join("a/b/c", "./openapi/schemas/X.yaml"),
            "a/b/c/openapi/schemas/X.yaml"
        );
        assert_eq!(join("a/b", "x//y"), "a/b/x/y");
    }

    #[test]
    fn join_rel_resolves_dotdot() {
        assert_eq!(join("a/b", "../c/file"), "a/c/file");
    }

    #[test]
    fn join_rel_handles_empty_sides() {
        assert_eq!(join("", "./x/y"), "x/y");
        assert_eq!(join("a/b", ""), "a/b");
        assert_eq!(join("", ""), "");
    }

    #[test]
    fn join_rel_preserves_trailing_slash() {
        assert_eq!(join("a/b", "gen/"), "a/b/gen/");
        // `./` is a directory reference to the package itself — slash kept.
        assert_eq!(join("a", "./"), "a/");
        // A fully empty rel has no trailing slash to preserve.
        assert_eq!(join("a", ""), "a");
    }

    #[test]
    fn join_rel_preserves_glob_metachars() {
        assert_eq!(join("a/b", "./gen/**/*.go"), "a/b/gen/**/*.go");
    }

    #[test]
    fn join_rel_checked_errors_on_escape() {
        let err = join_rel_checked("a", "../../x").unwrap_err();
        assert!(err.to_string().contains("escapes workspace root"), "{err}");
    }

    #[test]
    fn parent_packages_single_level() {
        let got: Vec<String> = pkg("a")
            .parent_packages()
            .map(|p| p.as_str().to_string())
            .collect();
        assert_eq!(got, vec!["a", ""]);
    }
}
