use std::fmt;
use std::path::Path;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PkgBuf(String);

impl PkgBuf {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub(crate) fn is_empty(&self) -> bool {
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

    #[test]
    fn parent_packages_single_level() {
        let got: Vec<String> = pkg("a")
            .parent_packages()
            .map(|p| p.as_str().to_string())
            .collect();
        assert_eq!(got, vec!["a", ""]);
    }
}
