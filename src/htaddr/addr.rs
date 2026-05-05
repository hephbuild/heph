use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use crate::htpkg::PkgBuf;
use itertools::Itertools;
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Addr {
    pub package: PkgBuf,
    pub name: String,
    pub args: HashMap<String, String>,
}

impl Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.args.is_empty() {
            write!(f, "//{}:{}", self.package, self.name)
        } else {
            write!(f, "//{}:{} SOMEARGSTODO", self.package, self.name)
        }
    }
}

impl Default for Addr {
    fn default() -> Self {
        Addr {
            package: PkgBuf::from(""),
            name: String::new(),
            args: HashMap::new(),
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
        for (k, v) in self.args.iter().sorted() {
            Hasher::write(h, k.as_bytes());
            Hasher::write(h, v.as_bytes());
        };
    }
}
