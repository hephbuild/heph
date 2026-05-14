use crate::htpkg::PkgBuf;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Addr {
    pub package: PkgBuf,
    pub name: String,
    pub args: BTreeMap<String, String>,
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
        Addr {
            package: PkgBuf::from(""),
            name: String::new(),
            args: BTreeMap::new(),
        }
    }
}

impl Addr {
    pub fn format(&self) -> String {
        format!("{}", self)
    }

    pub fn hash_str(&self) -> String {
        let mut h = Xxh3Default::new();
        self.hash(&mut h);
        format!("{:x}", h.digest())
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
