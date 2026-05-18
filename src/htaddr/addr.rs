use crate::htpkg::PkgBuf;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Debug, PartialEq, Eq)]
pub struct AddrInner {
    pub package: PkgBuf,
    pub name: String,
    pub args: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct Addr(Arc<AddrInner>);

impl Addr {
    pub fn new(package: PkgBuf, name: String, args: BTreeMap<String, String>) -> Self {
        Self(Arc::new(AddrInner {
            package,
            name,
            args,
        }))
    }

    /// Mutably access the inner addr, cloning if shared.
    pub fn make_mut(&mut self) -> &mut AddrInner {
        Arc::make_mut(&mut self.0)
    }

    pub fn format(&self) -> String {
        format!("{}", self)
    }

    pub fn hash_str(&self) -> String {
        let mut h = Xxh3Default::new();
        self.hash(&mut h);
        format!("{:x}", h.digest())
    }
}

impl Deref for Addr {
    type Target = AddrInner;

    #[inline]
    fn deref(&self) -> &AddrInner {
        &self.0
    }
}

impl Debug for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&*self.0, f)
    }
}

impl PartialEq for Addr {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl Eq for Addr {}

impl Clone for AddrInner {
    fn clone(&self) -> Self {
        Self {
            package: self.package.clone(),
            name: self.name.clone(),
            args: self.args.clone(),
        }
    }
}

impl Serialize for Addr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.format())
    }
}

impl Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "//{}:{}", self.package, self.name)?;
        if !self.args.is_empty() {
            write!(f, "@")?;
            for (i, (k, v)) in self.args.iter().enumerate() {
                if i > 0 {
                    write!(f, ",")?;
                }
                // Quote values that contain chars that would break bare_value parsing
                if v.is_empty() || v.contains([' ', ',', '|', '"']) {
                    write!(f, "{}=\"{}\"", k, v)?;
                } else {
                    write!(f, "{}={}", k, v)?;
                }
            }
        }
        Ok(())
    }
}

impl Default for Addr {
    fn default() -> Self {
        Addr::new(PkgBuf::from(""), String::new(), BTreeMap::new())
    }
}

impl Hash for Addr {
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hasher::write(h, self.package.as_bytes());
        Hasher::write(h, self.name.as_bytes());
        for (k, v) in &self.args {
            Hasher::write(h, k.as_bytes());
            Hasher::write(h, v.as_bytes());
        }
    }
}
