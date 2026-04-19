use std::fmt;
use std::ops::Deref;
use std::path::Path;

#[repr(transparent)]
pub struct Pkg(str);

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct PkgBuf(String);

impl Pkg {
    pub fn new(s: &str) -> &Pkg {
        // SAFETY: Pkg is repr(transparent) over str
        unsafe { &*(s as *const str as *const Pkg) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn has_prefix(&self, prefix: &Pkg) -> bool {
        let p = prefix.as_str();
        if p.is_empty() {
            return true;
        }
        self.0 == *p || self.0.starts_with(&format!("{}/", p))
    }
}

impl PartialEq for Pkg {
    fn eq(&self, other: &Pkg) -> bool {
        self.0 == other.0
    }
}

impl Eq for Pkg {}

impl fmt::Display for Pkg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for PkgBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PkgBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PkgBuf({:?})", self.0)
    }
}

impl Deref for PkgBuf {
    type Target = Pkg;

    fn deref(&self) -> &Pkg {
        Pkg::new(&self.0)
    }
}

impl AsRef<Pkg> for PkgBuf {
    fn as_ref(&self) -> &Pkg {
        self
    }
}

impl AsRef<str> for PkgBuf {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for Pkg {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
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
        assert!(pkg("foo/bar").has_prefix(pkg("foo/bar").as_ref()));
    }

    #[test]
    fn child_match() {
        assert!(pkg("foo/bar/baz").has_prefix(pkg("foo/bar").as_ref()));
        assert!(pkg("foo/bar/baz").has_prefix(pkg("foo").as_ref()));
    }

    #[test]
    fn no_partial_component_match() {
        assert!(!pkg("foo/bar/baz").has_prefix(pkg("foo/ba").as_ref()));
    }

    #[test]
    fn empty_prefix_matches_all() {
        assert!(pkg("").has_prefix(pkg("").as_ref()));
        assert!(pkg("foo/bar").has_prefix(pkg("").as_ref()));
        assert!(pkg("foo/bar/baz").has_prefix(pkg("").as_ref()));
    }
}
