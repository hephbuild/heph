use std::collections::HashMap;
use std::hash::Hasher;
use crate::htpkg::PkgBuf;
use itertools::Itertools;
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Addr {
    pub package: PkgBuf,
    pub name: String,
    pub args: HashMap<String, String>,
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
        format!("//{}:{}", self.package, self.name)
    }

    pub fn hash(&self) -> u64 {
        let mut h = Xxh3Default::new();
        Hasher::write(&mut h, self.package.as_bytes());
        Hasher::write(&mut h, self.name.as_bytes());
        for (k, v) in self.args.iter().sorted() {
            Hasher::write(&mut h, k.as_bytes());
            Hasher::write(&mut h, v.as_bytes());
        }
        h.digest()
    }

    pub fn hash_str(&self) -> String {
        format!("{:x}", self.hash())
    }
}
