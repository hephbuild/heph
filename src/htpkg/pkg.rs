use std::ops::Deref;

#[repr(transparent)]
pub struct Pkg(str);

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

impl PkgBuf {
    pub fn new(s: String) -> PkgBuf {
        PkgBuf(s)
    }
}

impl Deref for PkgBuf {
    type Target = Pkg;

    fn deref(&self) -> &Pkg {
        Pkg::new(&self.0)
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

impl AsRef<Pkg> for PkgBuf {
    fn as_ref(&self) -> &Pkg {
        self
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
