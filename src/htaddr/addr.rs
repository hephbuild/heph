use std::collections::HashMap;
use crate::htpkg::PkgBuf;

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
}
